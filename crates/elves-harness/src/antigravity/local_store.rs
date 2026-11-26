//! Antigravity 本地配置与本地会话历史读取工具（本机 Windows 与 WSL 共用）。
//!
//! 边界说明（见 AGENTS.md）：
//! - 调用官方运行时与上游在线能力必须经 `agy acp`；
//! - 本地用户配置文件（`~/.gemini/GEMINI.md`、`~/.gemini/config/mcp_config.json`、
//!   `~/.gemini/config/hooks.json`、`~/.gemini/config/skills/`、`~/.gemini/antigravity/skills/`）
//!   以及本地已落盘对话日志（`~/.gemini/{antigravity,antigravity-cli,antigravity-ide}/brain/<id>/.system_generated/logs/transcript[_full].jsonl`）
//!   仅涉及本机/WSL 本地文件，可直接读写，读写时严格校验路径边界与并发版本。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use super::super::{ApiError, HarnessAuthorization, permissions};

const MAX_TRANSCRIPT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SKILL_BYTES: u64 = 64 * 1024;

/// Antigravity 三个官方产品形态对应的本地数据子目录（见官方 customization hooks 文档）。
const PRODUCT_DATA_DIRS: &[&str] = &["antigravity", "antigravity-cli", "antigravity-ide"];

/// Antigravity `hooks.json` 官方支持的生命周期事件键。
const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "PreToolUse",
    "PostToolUse",
    "PreInvocation",
    "PostInvocation",
    "Stop",
];

#[derive(Debug, Clone)]
pub(crate) struct LocalTranscript {
    pub turns: Vec<Value>,
    pub model: String,
    pub effort: String,
    pub token_usage: Value,
    pub mtime_ms: u64,
    pub file_len: u64,
}

pub(crate) fn valid_native_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !matches!(id, "." | "..")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// 校验官方 `agy acp --app-data-dir` 允许的产品数据子目录白名单，禁止传入任意路径。
pub(crate) fn valid_app_data_dir(dir: &str) -> bool {
    PRODUCT_DATA_DIRS.contains(&dir)
}

/// 按本地会话库归属检测指定原生会话 ID 实际所属的 Antigravity 数据目录（`antigravity-cli` / `antigravity` / `antigravity-ide`）。
///
/// 背景：`agy acp` 默认使用 `--app-data-dir antigravity-cli`，但其 `session/list` 会跨目录列出桌面 App
/// （`~/.gemini/antigravity/conversations/<id>.db`）中的历史会话；若恢复/续聊时不传对应 `--app-data-dir`，
/// `session/prompt` 会因找不到轨迹而报 `not_found: trajectory not found`。
pub(crate) fn detect_session_app_data_dir(
    gemini_root: &Path,
    native_id: &str,
) -> Option<&'static str> {
    if !valid_native_id(native_id) || !gemini_root.is_absolute() {
        return None;
    }
    let order = ["antigravity-cli", "antigravity", "antigravity-ide"];
    for dir in order {
        let conv_dir = gemini_root.join(dir).join("conversations");
        for ext in ["db", "pb"] {
            let candidate = conv_dir.join(format!("{native_id}.{ext}"));
            if let Ok(meta) = std::fs::symlink_metadata(&candidate)
                && !meta.file_type().is_symlink()
                && meta.is_file()
            {
                return Some(dir);
            }
        }
    }
    for dir in order {
        let brain = gemini_root.join(dir).join("brain").join(native_id);
        if let Ok(meta) = std::fs::symlink_metadata(&brain)
            && !meta.file_type().is_symlink()
            && meta.is_dir()
        {
            return Some(dir);
        }
    }
    None
}

/// 本机只读核对 Antigravity 会话是否正被其他进程（Antigravity App / IDE / 终端 `agy` CLI / 其他 ACP 进程）占用。
///
/// 判定依据（经本机实测确认，绝不新建文件或改写任何字节）：
/// 1. `~/.gemini/<dir>/presence/<native_id>.lock` 若存在且已被外部进程持有 OS 排他锁（Antigravity App/IDE 活跃打开）；
/// 2. Windows 下 `~/.gemini/<dir>/conversations/<native_id>.db` 若存在且以 `share_mode(0)` 只读打开触发共享冲突
///    （`ERROR_SHARING_VIOLATION=32` / `ERROR_LOCK_VIOLATION=33`，覆盖 `agy` CLI、其他 `agy acp` 与桌面 App 持有句柄）。
pub(crate) fn is_local_session_occupied(gemini_root: &Path, native_id: &str) -> bool {
    if !valid_native_id(native_id) || !gemini_root.is_absolute() {
        return false;
    }
    for dir in PRODUCT_DATA_DIRS {
        // 1) 核对 presence/<native_id>.lock 的真实 OS 文件锁（未加锁的残留空文件不算占用）。
        let lock_path = gemini_root
            .join(dir)
            .join("presence")
            .join(format!("{native_id}.lock"));
        if let Ok(meta) = std::fs::symlink_metadata(&lock_path)
            && !meta.file_type().is_symlink()
            && meta.is_file()
        {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
            {
                Ok(file) => match file.try_lock() {
                    Ok(()) => {
                        let _ = file.unlock();
                    }
                    Err(std::fs::TryLockError::WouldBlock) => return true,
                    Err(std::fs::TryLockError::Error(err))
                        if matches!(err.raw_os_error(), Some(32 | 33)) =>
                    {
                        return true;
                    }
                    Err(_) => {}
                },
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || matches!(err.raw_os_error(), Some(32 | 33)) =>
                {
                    return true;
                }
                Err(_) => {}
            }
        }

        // 2) Windows 下以只读且不共享模式探测 conversations/<native_id>.db 句柄占用（不读写内容，立即释放）。
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let db_path = gemini_root
                .join(dir)
                .join("conversations")
                .join(format!("{native_id}.db"));
            if let Ok(meta) = std::fs::symlink_metadata(&db_path)
                && !meta.file_type().is_symlink()
                && meta.is_file()
            {
                match std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(0)
                    .open(&db_path)
                {
                    Ok(file) => drop(file),
                    Err(err) if matches!(err.raw_os_error(), Some(32 | 33)) => {
                        return true;
                    }
                    Err(_) => {}
                }
            }
        }
    }
    false
}

/// WSL 内只读核对指定 Antigravity 会话是否正被其他进程占用（检查 `presence/<id>.lock` 的 `flock` 与 `/proc/*/fd` 打开的 `.db`）。
pub(crate) async fn is_wsl_session_occupied(
    distribution: &str,
    user: &str,
    native_id: &str,
) -> bool {
    if !cfg!(windows)
        || !valid_native_id(native_id)
        || !super::super::preferences::valid_distribution_name(distribution)
        || user.is_empty()
        || user.contains('\0')
    {
        return false;
    }
    const SCRIPT: &str = r#"import fcntl, os, sys
nid = sys.argv[1]
home = os.path.expanduser("~")
dirs = ("antigravity", "antigravity-cli", "antigravity-ide")
for d in dirs:
    lp = os.path.join(home, ".gemini", d, "presence", nid + ".lock")
    if os.path.isfile(lp) and not os.path.islink(lp):
        try:
            fd = os.open(lp, os.O_RDWR)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(fd, fcntl.LOCK_UN)
            except BlockingIOError:
                print("OCCUPIED:lock")
                sys.exit(0)
            finally:
                os.close(fd)
        except OSError:
            pass
targets = {os.path.realpath(os.path.join(home, ".gemini", d, "conversations", nid + ".db")) for d in dirs}
my_pid = str(os.getpid())
for pid in os.listdir("/proc"):
    if not pid.isdigit() or pid == my_pid:
        continue
    fd_dir = f"/proc/{pid}/fd"
    try:
        for name in os.listdir(fd_dir):
            try:
                dest = os.path.realpath(os.readlink(os.path.join(fd_dir, name)))
                if dest in targets:
                    print("OCCUPIED:fd")
                    sys.exit(0)
            except OSError:
                continue
    except OSError:
        continue
print("FREE")
"#;
    let mut command = tokio::process::Command::new("wsl.exe");
    command
        .args([
            "--distribution",
            distribution,
            "--user",
            user,
            "--exec",
            "python3",
            "-c",
            SCRIPT,
            native_id,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(crate::windows_create_hidden_process());
    let Ok(Ok(output)) =
        tokio::time::timeout(std::time::Duration::from_secs(4), command.output()).await
    else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with("OCCUPIED")
}

/// 查找某原生会话 ID 对应的本地 JSONL 转录文件（优先完整版 `transcript_full.jsonl`，超限时回退紧凑版）。
pub(crate) fn find_local_transcript_file(gemini_root: &Path, native_id: &str) -> Option<(PathBuf, u64, u64)> {
    if !valid_native_id(native_id) || !gemini_root.is_absolute() {
        return None;
    }
    let mut best: Option<(PathBuf, u64, u64)> = None;
    for dir in PRODUCT_DATA_DIRS {
        let logs = gemini_root
            .join(dir)
            .join("brain")
            .join(native_id)
            .join(".system_generated")
            .join("logs");
        for filename in ["transcript_full.jsonl", "transcript.jsonl"] {
            let candidate = logs.join(filename);
            let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.is_file() || meta.len() == 0 {
                continue;
            }
            if meta.len() > MAX_TRANSCRIPT_BYTES {
                continue;
            }
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if best.as_ref().is_none_or(|(_, prev_mtime, _)| mtime_ms > *prev_mtime) {
                best = Some((candidate, mtime_ms, meta.len()));
            }
            // 同目录下优先选 transcript_full.jsonl，命中即不再降级读紧凑版。
            break;
        }
    }
    best
}

/// 直接读取本地 Antigravity JSONL 转录并投影为 Elves 轮次列表（仅只读本地文件，不阻塞于 ACP 冷启动）。
pub(crate) fn read_local_transcript(gemini_root: &Path, native_id: &str) -> Option<LocalTranscript> {
    let (path, mtime_ms, file_len) = find_local_transcript_file(gemini_root, native_id)?;
    let text = std::fs::read_to_string(&path).ok()?;
    let mut parsed = parse_transcript_jsonl(&text);
    if parsed.turns.is_empty() {
        return None;
    }
    parsed.mtime_ms = mtime_ms;
    parsed.file_len = file_len;
    Some(parsed)
}

/// 解析 Antigravity `transcript.jsonl` / `transcript_full.jsonl` 内容为 Elves 轮次。
pub(crate) fn parse_transcript_jsonl(text: &str) -> LocalTranscript {
    let mut turns: Vec<Value> = Vec::new();
    let mut last_model = String::new();
    let mut last_effort = String::new();
    let mut last_token_usage = Value::Null;
    let mut cumulative_input = 0u64;
    let mut cumulative_cached = 0u64;
    let mut cumulative_output = 0u64;
    let mut cumulative_thought = 0u64;

    for (line_idx, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let step_index = entry["step_index"]
            .as_u64()
            .unwrap_or(line_idx as u64);
        let ts_ms = entry["created_at"]
            .as_str()
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
            .unwrap_or(0);
        let entry_type = entry["type"].as_str().unwrap_or("");
        let source = entry["source"].as_str().unwrap_or("");

        match entry_type {
            "USER_INPUT" => {
                let raw_content = entry["content"].as_str().unwrap_or("");
                let user_text = extract_user_request_text(raw_content);
                if user_text.is_empty() {
                    continue;
                }
                turns.push(json!({
                    "id": format!("agy-local-turn-{step_index}"),
                    "status": "completed",
                    "createdAt": ts_ms,
                    "completedAt": ts_ms,
                    "turnStartedAtMs": ts_ms,
                    "turnCompletedAtMs": ts_ms,
                    "items": [{
                        "type": "userMessage",
                        "id": format!("agy-local-user-{step_index}"),
                        "content": [{ "type": "text", "text": user_text }],
                    }],
                }));
            }
            "PLANNER_RESPONSE" => {
                if let Some(model_raw) = entry["model"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    let (base, effort) = split_model_effort(model_raw);
                    last_model = base;
                    if !effort.is_empty() {
                        last_effort = effort;
                    }
                }
                let uncached_input = entry["input_tokens"].as_u64().unwrap_or(0);
                let cache_read = entry["cache_read_tokens"].as_u64().unwrap_or(0);
                let output_tokens = entry["output_tokens"].as_u64().unwrap_or(0);
                let thought_tokens = entry["thinking_tokens"]
                    .as_u64()
                    .or_else(|| entry["thought_tokens"].as_u64())
                    .unwrap_or(0);
                let step_input = uncached_input.saturating_add(cache_read);
                let step_total = step_input.saturating_add(output_tokens);
                if step_total > 0 {
                    cumulative_input = cumulative_input.saturating_add(step_input);
                    cumulative_cached = cumulative_cached.saturating_add(cache_read);
                    cumulative_output = cumulative_output.saturating_add(output_tokens);
                    cumulative_thought = cumulative_thought.saturating_add(thought_tokens);
                    let cumulative_total = cumulative_input.saturating_add(cumulative_output);
                    let mut lu = json!({
                        "last": {
                            "totalTokens": step_total,
                            "inputTokens": step_input,
                            "uncachedInputTokens": uncached_input,
                            "cachedInputTokens": cache_read,
                            "outputTokens": output_tokens,
                            "reasoningOutputTokens": thought_tokens,
                        },
                        "total": {
                            "totalTokens": cumulative_total,
                            "inputTokens": cumulative_input,
                            "cachedInputTokens": cumulative_cached,
                            "outputTokens": cumulative_output,
                            "reasoningOutputTokens": cumulative_thought,
                        }
                    });
                    if let Some(window) = known_antigravity_context_window(&last_model) {
                        lu["modelContextWindow"] = json!(window);
                        lu["modelContextWindowSource"] = json!({
                            "kind": "model-default",
                            "key": last_model,
                        });
                    }
                    last_token_usage = lu;
                }

                let thinking = entry["thinking"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let content = entry["content"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let tool_calls = entry["tool_calls"]
                    .as_array()
                    .filter(|calls| !calls.is_empty());

                if thinking.is_none() && content.is_none() && tool_calls.is_none() && step_total == 0 {
                    continue;
                }

                if turns.is_empty() {
                    turns.push(json!({
                        "id": format!("agy-local-turn-{step_index}"),
                        "status": "completed",
                        "createdAt": ts_ms,
                        "completedAt": ts_ms,
                        "turnStartedAtMs": ts_ms,
                        "turnCompletedAtMs": ts_ms,
                        "items": [],
                    }));
                }
                let turn = turns.last_mut().unwrap();
                if ts_ms > 0 {
                    if turn["firstTokenAtMs"].is_null() {
                        turn["firstTokenAtMs"] = json!(ts_ms);
                    }
                    turn["completedAt"] = json!(ts_ms);
                    turn["turnCompletedAtMs"] = json!(ts_ms);
                }
                let prev = turn.get("_acpUsage").cloned().unwrap_or_else(|| json!({}));
                let prev_steps = prev["steps"].as_u64().unwrap_or(0);
                let prev_in = prev["inputTokens"].as_u64().unwrap_or(0);
                let prev_uncached = prev["uncachedInputTokens"].as_u64().unwrap_or(0);
                let prev_cached = prev["cachedReadTokens"].as_u64().unwrap_or(0);
                let prev_out = prev["outputTokens"].as_u64().unwrap_or(0);
                let prev_thought = prev["thoughtTokens"].as_u64().unwrap_or(0);
                let prev_total = prev["totalTokens"].as_u64().unwrap_or(0);
                // 用户要求按实际模型拆分本轮费用；保留每次原生调用，不能用最后一次模型覆盖前面的调用。
                let mut billing_parts = prev["billingParts"].as_array().cloned().unwrap_or_default();
                let mut billing_complete = prev["billingComplete"] != false;
                if billing_parts.len() < crate::session_metrics_cost::MAX_PARTS {
                    let actual_model = entry["model"].as_str()
                        .map(|model| split_model_effort(model).0).unwrap_or_default();
                    let input = entry["input_tokens"].as_u64().zip(entry["cache_read_tokens"].as_u64())
                        .and_then(|(input, cached)| input.checked_add(cached));
                    billing_parts.push(crate::session_metrics_cost::responses_part(
                        &actual_model,
                        &json!({"inputTokens":input,"outputTokens":entry["output_tokens"],
                            "cachedInputTokens":entry["cache_read_tokens"],
                            "reasoningOutputTokens":entry.get("thinking_tokens").or_else(||entry.get("thought_tokens"))}),
                        true,
                        &Value::Null,
                    ));
                } else {
                    billing_complete = false;
                }
                turn["_acpUsage"] = json!({
                    "steps": prev_steps.saturating_add(1),
                    "inputTokens": prev_in.saturating_add(step_input),
                    "uncachedInputTokens": prev_uncached.saturating_add(uncached_input),
                    "cachedReadTokens": prev_cached.saturating_add(cache_read),
                    "outputTokens": prev_out.saturating_add(output_tokens),
                    "thoughtTokens": prev_thought.saturating_add(thought_tokens),
                    "totalTokens": prev_total.saturating_add(step_total),
                    "lastObservedAtMs": if ts_ms > 0 { json!(ts_ms) } else { prev["lastObservedAtMs"].clone() },
                    "model": if !last_model.is_empty() { json!(last_model) } else { prev["model"].clone() },
                    "billingParts": billing_parts,
                    "billingComplete": billing_complete,
                });
                let items = turn["items"].as_array_mut().unwrap();
                if let Some(thought) = thinking {
                    items.push(json!({
                        "id": format!("agy-local-thought-{step_index}"),
                        "type": "reasoning",
                        "summary": [],
                        "content": [thought],
                        "status": "completed",
                    }));
                }
                if let Some(calls) = tool_calls {
                    for (idx, call) in calls.iter().enumerate() {
                        if let Some(item) = project_local_tool_call(step_index, idx, call) {
                            items.push(item);
                        }
                    }
                }
                if let Some(reply) = content {
                    items.push(json!({
                        "id": format!("agy-local-msg-{step_index}"),
                        "type": "agentMessage",
                        "text": reply,
                        "status": "completed",
                    }));
                }
            }
            "ERROR_MESSAGE" => {
                if let Some(msg) = entry["content"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    && let Some(turn) = turns.last_mut()
                {
                    turn["error"] = json!({ "message": msg });
                }
            }
            _ if source == "USER" => {
                let user_text = extract_user_request_text(entry["content"].as_str().unwrap_or(""));
                if !user_text.is_empty() {
                    turns.push(json!({
                        "id": format!("agy-local-turn-{step_index}"),
                        "status": "completed",
                        "createdAt": ts_ms,
                        "completedAt": ts_ms,
                        "turnStartedAtMs": ts_ms,
                        "turnCompletedAtMs": ts_ms,
                        "items": [{
                            "type": "userMessage",
                            "id": format!("agy-local-user-{step_index}"),
                            "content": [{ "type": "text", "text": user_text }],
                        }],
                    }));
                }
            }
            _ => {}
        }
    }

    LocalTranscript {
        turns,
        model: last_model,
        effort: last_effort,
        token_usage: last_token_usage,
        mtime_ms: 0,
        file_len: 0,
    }
}

fn extract_user_request_text(raw: &str) -> String {
    if let Some(start) = raw.find("<USER_REQUEST>") {
        let after = &raw[start + "<USER_REQUEST>".len()..];
        let body = after
            .find("</USER_REQUEST>")
            .map(|end| &after[..end])
            .unwrap_or(after);
        return body.trim().to_owned();
    }
    let without_meta = raw
        .split("<ADDITIONAL_METADATA>")
        .next()
        .unwrap_or(raw)
        .split("<USER_SETTINGS>")
        .next()
        .unwrap_or(raw);
    without_meta.trim().to_owned()
}

pub(crate) fn split_model_effort(model: &str) -> (String, String) {
    for suffix in ["-xhigh", "-high", "-medium", "-low", "-max", "-minimal"] {
        if let Some(base) = model.strip_suffix(suffix)
            && !base.is_empty()
        {
            return (base.to_owned(), suffix[1..].to_owned());
        }
    }
    (model.to_owned(), String::new())
}

/// 已知 Antigravity 模型上下文容量表：在未收到实时 `acp.usage_update.size` 前提供准确容量以渲染环形进度，
/// 实时 ACP 返回 `size` 后仍优先采用原生容量。
pub(crate) fn known_antigravity_context_window(model: &str) -> Option<u64> {
    let lower = model.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lower.starts_with("gemini-") || lower.starts_with("orchestrator") {
        Some(1_048_576)
    } else if lower.starts_with("claude-") {
        Some(200_000)
    } else if lower.starts_with("gpt-oss-") {
        Some(131_072)
    } else {
        None
    }
}

fn project_local_tool_call(step_index: u64, idx: usize, call: &Value) -> Option<Value> {
    let name = call["name"].as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
    let id = format!("agy-local-tool-{step_index}-{idx}");
    match name {
        "run_command" => {
            let cmd = args["CommandLine"]
                .as_str()
                .or_else(|| args["command"].as_str())
                .unwrap_or("");
            Some(json!({
                "id": id,
                "type": "commandExecution",
                "command": cmd,
                "status": "completed",
            }))
        }
        "edit_file" | "write_file" | "replace_file_content" | "multi_replace_file_content" | "write_to_file" => {
            let path = args["Path"]
                .as_str()
                .or_else(|| args["TargetFile"].as_str())
                .or_else(|| args["path"].as_str())
                .unwrap_or("");
            let is_create = matches!(name, "write_file" | "write_to_file");
            let changes = if path.is_empty() {
                vec![]
            } else {
                vec![json!({
                    "path": path,
                    "kind": if is_create { "create" } else { "update" }
                })]
            };
            Some(json!({
                "id": id,
                "type": "fileChange",
                "changes": changes,
                "status": "completed",
            }))
        }
        "search_web" => {
            let query = args["query"].as_str().unwrap_or("");
            Some(json!({
                "id": id,
                "type": "webSearch",
                "query": query,
                "status": "completed",
            }))
        }
        _ if name.starts_with("mcp__") || name.starts_with("mcp_") => {
            let stripped = name
                .strip_prefix("mcp__")
                .or_else(|| name.strip_prefix("mcp_"))
                .unwrap_or(name);
            let (server, tool) = stripped
                .split_once("__")
                .or_else(|| stripped.split_once('_'))
                .unwrap_or(("mcp", stripped));
            Some(json!({
                "id": id,
                "type": "mcpToolCall",
                "server": server,
                "tool": tool,
                "arguments": args,
                "status": "completed",
            }))
        }
        // Antigravity 内置工具（如 read_file、grep_search、find_by_name、list_dir、invoke_subagent 等）
        // 统一投影为 `source: "antigravity"` 的 `dynamicToolCall`，避免在活动折叠行误报为“已使用 antigravity 集成”。
        _ => Some(json!({
            "id": id,
            "type": "dynamicToolCall",
            "source": "antigravity",
            "tool": name,
            "arguments": args,
            "status": "completed",
        })),
    }
}

/// 读取 `~/.gemini/google_accounts.json` 中记录的当前登录展示邮箱（`active` 字段）。
///
/// 安全边界：该文件仅存放账号邮箱文本（`{"active":"...","old":[...]}`），不含任何 OAuth Token 或凭据；
/// 严格校验非符号链接、文件体积上限及邮箱格式，绝不读取 `oauth_creds.json`。
pub(crate) fn read_google_account_email(gemini_root: &Path) -> Option<String> {
    if !gemini_root.is_absolute() {
        return None;
    }
    let path = gemini_root.join("google_accounts.json");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_SKILL_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let doc: Value = serde_json::from_slice(&bytes).ok()?;
    let email = doc["active"].as_str()?.trim();
    if email.is_empty()
        || email.len() > 320
        || email.matches('@').count() != 1
        || email
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return None;
    }
    Some(email.to_owned())
}

pub(crate) fn valid_slash_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 256
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | ':' | '-'))
}

/// 扫描当前会话/工作区及用户级目录下的可用 Skills，格式化为输入区 `/` 菜单条目。
pub(crate) fn scan_slash_skills(
    gemini_root: &Path,
    user_home_root: Option<&Path>,
    workspace_root: Option<&Path>,
) -> Vec<Value> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(ws) = workspace_root.filter(|p| p.is_absolute()) {
        dirs.push(ws.join(".gemini").join("skills"));
        dirs.push(ws.join(".agents").join("skills"));
    }
    if gemini_root.is_absolute() {
        dirs.push(gemini_root.join("config").join("skills"));
        dirs.push(gemini_root.join("antigravity").join("skills"));
        dirs.push(gemini_root.join("antigravity-cli").join("skills"));
    }
    if let Some(home) = user_home_root.filter(|p| p.is_absolute()) {
        dirs.push(home.join(".agents").join("skills"));
    }
    let mut seen = BTreeSet::new();
    let mut rows: Vec<(String, Value)> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let folder = entry.path();
            let Ok(folder_meta) = std::fs::symlink_metadata(&folder) else {
                continue;
            };
            if folder_meta.file_type().is_symlink() || !folder_meta.is_dir() {
                continue;
            }
            let skill_file = folder.join("SKILL.md");
            let Ok(file_meta) = std::fs::symlink_metadata(&skill_file) else {
                continue;
            };
            if file_meta.file_type().is_symlink()
                || !file_meta.is_file()
                || file_meta.len() > MAX_SKILL_BYTES
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&skill_file) else {
                continue;
            };
            let folder_name = entry.file_name().to_string_lossy().into_owned();
            let (parsed_name, description) = parse_skill_frontmatter(&text);
            let name = if parsed_name.is_empty() {
                folder_name
            } else {
                parsed_name
            };
            if !valid_slash_command_name(&name) {
                continue;
            }
            let dedup_key = name.to_ascii_lowercase();
            if matches!(dedup_key.as_str(), "goal" | "compact") || !seen.insert(dedup_key.clone()) {
                continue;
            }
            rows.push((
                dedup_key,
                json!({
                    "name": name,
                    "command": name,
                    "path": format!("antigravity-skill:{name}"),
                    "description": description,
                    "argumentHint": "",
                    "backend": "antigravity",
                    "source": "antigravity",
                }),
            ));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter().map(|(_, v)| v).collect()
}

/// 扫描所选 Antigravity 实例的本地用户级 Skills 目录（遵循官方定制路径，不读取项目或凭据）。
pub(crate) fn scan_local_skills(
    gemini_root: &Path,
    user_home_root: Option<&Path>,
) -> Result<Value, ApiError> {
    let mut roots: Vec<(&str, PathBuf)> = vec![
        ("config", gemini_root.join("config").join("skills")),
        ("antigravity", gemini_root.join("antigravity").join("skills")),
        ("cli", gemini_root.join("antigravity-cli").join("skills")),
    ];
    if let Some(home) = user_home_root {
        roots.push(("agents", home.join(".agents").join("skills")));
    }

    let mut seen = BTreeSet::new();
    let mut rows: Vec<(String, Value)> = Vec::new();
    for (scope, dir) in roots {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let folder = entry.path();
            let Ok(folder_meta) = std::fs::symlink_metadata(&folder) else {
                continue;
            };
            if folder_meta.file_type().is_symlink() || !folder_meta.is_dir() {
                continue;
            }
            let skill_file = folder.join("SKILL.md");
            let Ok(file_meta) = std::fs::symlink_metadata(&skill_file) else {
                continue;
            };
            if file_meta.file_type().is_symlink()
                || !file_meta.is_file()
                || file_meta.len() > MAX_SKILL_BYTES
            {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&skill_file) else {
                continue;
            };
            let folder_name = entry.file_name().to_string_lossy().into_owned();
            let (parsed_name, description) = parse_skill_frontmatter(&text);
            let name = if parsed_name.is_empty() {
                folder_name
            } else {
                parsed_name
            };
            let dedup_key = name.to_ascii_lowercase();
            if name.is_empty() || !seen.insert(dedup_key.clone()) {
                continue;
            }
            rows.push((
                dedup_key,
                json!({
                    "id": format!("antigravity-skill:{name}"),
                    "name": name,
                    "description": description,
                    "scope": scope,
                    "enabled": true,
                    "canToggle": false,
                    "readOnlyReason": "目录发现",
                }),
            ));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let data: Vec<Value> = rows.into_iter().map(|(_, row)| row).collect();
    Ok(json!({
        "data": data,
        "message": "扫描所选实例用户目录中的 Skills（~/.gemini/config/skills、~/.gemini/antigravity/skills 与 ~/.agents/skills）；目录内 Skill 默认自动生效。",
    }))
}

fn parse_skill_frontmatter(text: &str) -> (String, String) {
    let trimmed = text.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed
        .strip_prefix("---\r\n")
        .or_else(|| trimmed.strip_prefix("---\n"))
    else {
        return (String::new(), String::new());
    };
    let mut name = String::new();
    let mut description = String::new();
    let mut in_multiline_desc = false;
    let mut desc_lines: Vec<String> = Vec::new();
    for line in rest.lines() {
        let stripped = line.trim_end_matches('\r');
        if stripped.trim() == "---" {
            break;
        }
        if in_multiline_desc {
            if stripped.starts_with(' ') || stripped.starts_with('\t') || stripped.trim().is_empty() {
                let part = stripped.trim();
                if !part.is_empty() {
                    desc_lines.push(part.to_owned());
                }
                continue;
            }
            in_multiline_desc = false;
        }
        if let Some(val) = stripped.strip_prefix("name:") {
            name = unquote_yaml_scalar(val);
        } else if let Some(val) = stripped.strip_prefix("description:") {
            let trimmed_val = val.trim();
            if matches!(trimmed_val, ">" | ">-" | "|" | "|-") {
                in_multiline_desc = true;
            } else {
                description = unquote_yaml_scalar(trimmed_val);
            }
        }
    }
    if description.is_empty() && !desc_lines.is_empty() {
        description = desc_lines.join(" ");
    }
    if description.chars().count() > 260 {
        description = format!("{}…", description.chars().take(259).collect::<String>());
    }
    (name, description)
}

fn unquote_yaml_scalar(raw: &str) -> String {
    let s = raw.trim();
    if s.len() >= 2
        && ((s.starts_with('\'') && s.ends_with('\''))
            || (s.starts_with('"') && s.ends_with('"')))
    {
        return s[1..s.len() - 1].trim().to_owned();
    }
    s.to_owned()
}

pub(crate) fn mcp_config_path(gemini_root: &Path) -> PathBuf {
    gemini_root.join("config").join("mcp_config.json")
}

pub(crate) fn hooks_config_path(gemini_root: &Path) -> PathBuf {
    gemini_root.join("config").join("hooks.json")
}

/// 读取 Antigravity 用户级 `~/.gemini/config/mcp_config.json`（只返回名称、传输方式与启停状态，不暴露 env/headers/url 凭据）。
pub(crate) fn read_mcp_inventory(gemini_root: &Path) -> Result<(Value, String), ApiError> {
    let path = mcp_config_path(gemini_root);
    let (text, revision) = permissions::read_file(&path)?;
    let document: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|_| ApiError::new(422, "Antigravity mcp_config.json 格式无效，未覆盖原文件"))?
    };
    if !document.is_object() {
        return Err(ApiError::new(422, "Antigravity mcp_config.json 必须为 JSON 对象"));
    }
    let servers = match document.get("mcpServers") {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => Some(map),
        Some(_) => {
            return Err(ApiError::new(
                422,
                "Antigravity mcp_config.json 中的 mcpServers 必须为对象",
            ));
        }
    };
    let mut rows: Vec<(String, Value)> = servers
        .into_iter()
        .flatten()
        .filter(|(_, val)| val.is_object())
        .map(|(name, val)| {
            let enabled = if let Some(disabled) = val["disabled"].as_bool() {
                !disabled
            } else if let Some(enabled) = val["enabled"].as_bool() {
                enabled
            } else {
                true
            };
            let transport = if val["command"].is_string() {
                "stdio"
            } else if val["url"].is_string() || val["serverUrl"].is_string() {
                "http"
            } else {
                "未指定连接方式"
            };
            (
                name.to_ascii_lowercase(),
                json!({
                    "id": name,
                    "name": name,
                    "enabled": enabled,
                    "canToggle": true,
                    "transport": transport,
                }),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let data: Vec<Value> = rows.into_iter().map(|(_, row)| row).collect();
    Ok((
        json!({
            "data": data,
            "message": "仅管理所选实例的用户级 ~/.gemini/config/mcp_config.json；增删 MCP 服务可继续使用 agy mcp 命令。",
        }),
        revision,
    ))
}

/// 切换 `~/.gemini/config/mcp_config.json` 中已有 MCP 服务的启用状态，保留原文件其他键、环境变量与换行风格。
pub(crate) async fn update_mcp_server_enabled(
    gemini_root: &Path,
    expected_revision: &str,
    change: &Value,
    authorization: &HarnessAuthorization,
) -> Result<(), ApiError> {
    let id = change["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 200)
        .ok_or_else(|| ApiError::new(400, "请选择当前实例的 MCP 服务"))?;
    let enabled = change["enabled"]
        .as_bool()
        .ok_or_else(|| ApiError::new(400, "MCP 开关必须为布尔值"))?;
    let path = mcp_config_path(gemini_root);
    let (text, revision) = permissions::read_file(&path)?;
    if revision != expected_revision {
        return Err(ApiError::new(409, "Antigravity MCP 配置已变化，请重新读取后再保存"));
    }
    let mut document: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|_| ApiError::new(422, "Antigravity mcp_config.json 格式无效，未覆盖原文件"))?
    };
    let server = document["mcpServers"]
        .as_object_mut()
        .and_then(|map| map.get_mut(id))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::new(409, "MCP 服务已不在当前实例中，请重新读取"))?;
    if enabled {
        server.remove("disabled");
        if server.contains_key("enabled") {
            server.insert("enabled".into(), json!(true));
        }
    } else {
        server.insert("disabled".into(), json!(true));
        if server.contains_key("enabled") {
            server.insert("enabled".into(), json!(false));
        }
    }
    let output = format_json_preserving_style(&text, &document);
    let permit = authorization
        .prepare_dispatch()
        .await
        .map_err(super::super::authorization_error)?;
    permit
        .commit(|| permissions::save_file(&path, &output, &revision))
        .map_err(super::super::authorization_error)
}

/// 读取 Antigravity 用户级 `~/.gemini/config/hooks.json`（只返回具名钩子、监听事件和启停状态，不暴露 shell 命令原文）。
pub(crate) fn read_hooks_inventory(gemini_root: &Path) -> Result<(Value, String), ApiError> {
    let path = hooks_config_path(gemini_root);
    let (text, revision) = permissions::read_file(&path)?;
    let document: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|_| ApiError::new(422, "Antigravity hooks.json 格式无效，未覆盖原文件"))?
    };
    let Some(map) = document.as_object() else {
        return Err(ApiError::new(422, "Antigravity hooks.json 必须为 JSON 对象"));
    };
    let mut rows: Vec<(String, Value)> = map
        .iter()
        .filter(|(_, val)| val.is_object())
        .map(|(name, val)| {
            let enabled = val["enabled"].as_bool().unwrap_or(true);
            let mut events = Vec::new();
            let mut rule_count = 0usize;
            for event in HOOK_EVENTS {
                if let Some(arr) = val.get(*event).and_then(Value::as_array)
                    && !arr.is_empty()
                {
                    events.push(*event);
                    rule_count += arr.len();
                }
            }
            let description = if events.is_empty() {
                "未配置事件处理器".to_owned()
            } else {
                format!("{}（共 {} 条）", events.join("、"), rule_count)
            };
            (
                name.to_ascii_lowercase(),
                json!({
                    "id": name,
                    "name": name,
                    "description": description,
                    "enabled": enabled,
                    "canToggle": true,
                }),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let data: Vec<Value> = rows.into_iter().map(|(_, row)| row).collect();
    Ok((
        json!({
            "data": data,
            "enabled": Value::Null,
            "message": "仅管理所选实例的用户级 ~/.gemini/config/hooks.json；每个具名钩子支持独立启停，插件钩子继续由插件管理。",
        }),
        revision,
    ))
}

/// 切换 `~/.gemini/config/hooks.json` 中已有具名钩子的 `enabled` 字段。
pub(crate) async fn update_hook_enabled(
    gemini_root: &Path,
    expected_revision: &str,
    change: &Value,
    authorization: &HarnessAuthorization,
) -> Result<(), ApiError> {
    let id = change["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 200)
        .ok_or_else(|| ApiError::new(400, "请选择当前实例的钩子"))?;
    let enabled = change["enabled"]
        .as_bool()
        .ok_or_else(|| ApiError::new(400, "钩子开关必须为布尔值"))?;
    let path = hooks_config_path(gemini_root);
    let (text, revision) = permissions::read_file(&path)?;
    if revision != expected_revision {
        return Err(ApiError::new(409, "Antigravity 钩子配置已变化，请重新读取后再保存"));
    }
    let mut document: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|_| ApiError::new(422, "Antigravity hooks.json 格式无效，未覆盖原文件"))?
    };
    let hook = document
        .as_object_mut()
        .and_then(|map| map.get_mut(id))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::new(409, "该钩子已不在当前实例中，请重新读取"))?;
    hook.insert("enabled".into(), json!(enabled));
    let output = format_json_preserving_style(&text, &document);
    let permit = authorization
        .prepare_dispatch()
        .await
        .map_err(super::super::authorization_error)?;
    permit
        .commit(|| permissions::save_file(&path, &output, &revision))
        .map_err(super::super::authorization_error)
}

fn format_json_preserving_style(original: &str, document: &Value) -> String {
    let ending = if original.contains("\r\n") { "\r\n" } else { "\n" };
    let mut output = serde_json::to_string_pretty(document)
        .expect("JSON 序列化不应失败")
        .replace('\n', ending);
    if original.starts_with('\u{feff}') {
        output.insert(0, '\u{feff}');
    }
    output.push_str(ending);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_antigravity_transcript_jsonl_into_elves_turns() {
        let sample = concat!(
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-26T02:07:15Z","content":"<USER_REQUEST>\nAntigravity 是否支持自定义系统提示词\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-26T10:07:15+08:00.\n</ADDITIONAL_METADATA>"}"#, "\n",
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-26T02:07:16Z","model":"gemini-4-neon-xhigh","input_tokens":19520,"cache_read_tokens":0,"output_tokens":321,"thinking":"Checking customization docs","tool_calls":[{"name":"read_file","args":{"Path":"C:\\Users\\demo\\SKILL.md"}}]}"#, "\n",
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-26T02:07:17Z","content":"tool output"}"#, "\n",
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-26T02:07:20Z","model":"gemini-4-neon-xhigh","input_tokens":3000,"cache_read_tokens":19520,"output_tokens":500,"content":"支持，可通过 ~/.gemini/GEMINI.md 配置。"}"#, "\n",
            r#"{"step_index":4,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-26T02:11:07Z","content":"<USER_REQUEST>\n第二个问题\n</USER_REQUEST>"}"#, "\n",
            r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-26T02:11:10Z","model":"gemini-4-neon-high","content":"第二个回答"}"#, "\n"
        );
        let parsed = parse_transcript_jsonl(sample);
        assert_eq!(parsed.turns.len(), 2);
        assert_eq!(parsed.model, "gemini-4-neon");
        assert_eq!(parsed.effort, "high");
        assert_eq!(
            parsed.turns[0]["items"][0]["content"][0]["text"],
            "Antigravity 是否支持自定义系统提示词"
        );
        assert_eq!(parsed.turns[0]["items"][1]["type"], "reasoning");
        assert_eq!(parsed.turns[0]["items"][2]["type"], "dynamicToolCall");
        assert_eq!(parsed.turns[0]["items"][2]["source"], "antigravity");
        assert_eq!(parsed.turns[0]["items"][2]["tool"], "read_file");
        assert_eq!(parsed.turns[0]["_acpUsage"]["steps"], 2);
        assert_eq!(parsed.turns[0]["_acpUsage"]["totalTokens"], 42_861);
        assert_eq!(parsed.token_usage["modelContextWindow"], 1_048_576);
        assert_eq!(
            parsed.turns[0]["items"][3]["text"],
            "支持，可通过 ~/.gemini/GEMINI.md 配置。"
        );
        assert_eq!(parsed.turns[1]["items"][0]["content"][0]["text"], "第二个问题");
        assert_eq!(parsed.turns[1]["items"][1]["text"], "第二个回答");
    }

    #[test]
    fn scans_local_skills_and_reads_mcp_and_hooks_safely() {
        let dir = tempfile::tempdir().unwrap();
        let gemini = dir.path().join(".gemini");
        let skill_dir = gemini.join("antigravity").join("skills").join("demo-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo-skill\ndescription: 'Use for testing skill scan'\n---\n# Body\n",
        )
        .unwrap();
        let cfg = gemini.join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("mcp_config.json"),
            r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","server"],"env":{"SECRET":"do-not-leak"}},"api":{"url":"https://example.com/mcp","disabled":true}}}"#,
        )
        .unwrap();
        std::fs::write(
            cfg.join("hooks.json"),
            r#"{"lint-check":{"PostToolUse":[{"matcher":"run_command","hooks":[{"command":"./secret.sh"}]}]},"gate":{"enabled":false,"PreToolUse":[{"matcher":"*","hooks":[{"command":"./gate.sh"}]}]}}"#,
        )
        .unwrap();

        std::fs::write(
            gemini.join("google_accounts.json"),
            r#"{"active":"demo.user@example.com","old":[]}"#,
        )
        .unwrap();
        assert_eq!(
            read_google_account_email(&gemini).as_deref(),
            Some("demo.user@example.com")
        );
        let slash_skills = scan_slash_skills(&gemini, Some(dir.path()), None);
        assert_eq!(slash_skills.len(), 1);
        assert_eq!(slash_skills[0]["command"], "demo-skill");
        assert_eq!(slash_skills[0]["path"], "antigravity-skill:demo-skill");
        assert_eq!(slash_skills[0]["backend"], "antigravity");

        let skills = scan_local_skills(&gemini, Some(dir.path())).unwrap();
        assert_eq!(skills["data"].as_array().unwrap().len(), 1);
        assert_eq!(skills["data"][0]["name"], "demo-skill");
        assert_eq!(skills["data"][0]["description"], "Use for testing skill scan");

        let (mcp, _rev) = read_mcp_inventory(&gemini).unwrap();
        let mcp_json = serde_json::to_string(&mcp).unwrap();
        assert!(!mcp_json.contains("do-not-leak"));
        assert!(!mcp_json.contains("example.com"));
        let rows = mcp["data"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["name"], "api");
        assert_eq!(rows[0]["enabled"], false);
        assert_eq!(rows[0]["transport"], "http");
        assert_eq!(rows[1]["name"], "fs");
        assert_eq!(rows[1]["enabled"], true);
        assert_eq!(rows[1]["transport"], "stdio");

        let (hooks, _hrev) = read_hooks_inventory(&gemini).unwrap();
        let hooks_json = serde_json::to_string(&hooks).unwrap();
        assert!(!hooks_json.contains("secret.sh"));
        let hrows = hooks["data"].as_array().unwrap();
        assert_eq!(hrows.len(), 2);
        assert_eq!(hrows[0]["name"], "gate");
        assert_eq!(hrows[0]["enabled"], false);
        assert_eq!(hrows[1]["name"], "lint-check");
        assert_eq!(hrows[1]["enabled"], true);
    }

    #[test]
    fn detects_session_app_data_dir_across_cli_app_and_ide() {
        let dir = tempfile::tempdir().unwrap();
        let gemini = dir.path().join(".gemini");
        std::fs::create_dir_all(gemini.join("antigravity").join("conversations")).unwrap();
        std::fs::create_dir_all(gemini.join("antigravity-cli").join("conversations")).unwrap();
        std::fs::create_dir_all(gemini.join("antigravity-ide").join("brain").join("ide-1")).unwrap();
        std::fs::write(
            gemini.join("antigravity").join("conversations").join("app-1.db"),
            b"sqlite",
        )
        .unwrap();
        std::fs::write(
            gemini.join("antigravity-cli").join("conversations").join("cli-1.db"),
            b"sqlite",
        )
        .unwrap();

        assert_eq!(detect_session_app_data_dir(&gemini, "app-1"), Some("antigravity"));
        assert_eq!(detect_session_app_data_dir(&gemini, "cli-1"), Some("antigravity-cli"));
        assert_eq!(detect_session_app_data_dir(&gemini, "ide-1"), Some("antigravity-ide"));
        assert_eq!(detect_session_app_data_dir(&gemini, "missing-1"), None);
        assert_eq!(detect_session_app_data_dir(&gemini, "../escape"), None);
    }

    #[test]
    fn detects_local_occupancy_via_presence_lock_and_db_handle() {
        let dir = tempfile::tempdir().unwrap();
        let gemini = dir.path().join(".gemini");
        let presence = gemini.join("antigravity").join("presence");
        let convs = gemini.join("antigravity-cli").join("conversations");
        std::fs::create_dir_all(&presence).unwrap();
        std::fs::create_dir_all(&convs).unwrap();

        // 1) 残留未加锁的 .lock 文件不构成外部占用
        let lock_path = presence.join("app-sess-1.lock");
        std::fs::write(&lock_path, b"").unwrap();
        assert!(!is_local_session_occupied(&gemini, "app-sess-1"));

        // 持有排他锁后应立即判定为外部占用
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        owner.lock().unwrap();
        assert!(is_local_session_occupied(&gemini, "app-sess-1"));
        owner.unlock().unwrap();
        drop(owner);
        assert!(!is_local_session_occupied(&gemini, "app-sess-1"));

        // 2) Windows 下其他进程持有 conversations/<id>.db 句柄时也应判定为外部占用
        let db_path = convs.join("cli-sess-2.db");
        std::fs::write(&db_path, b"sqlite-placeholder").unwrap();
        assert!(!is_local_session_occupied(&gemini, "cli-sess-2"));
        #[cfg(windows)]
        {
            let holder = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&db_path)
                .unwrap();
            assert!(is_local_session_occupied(&gemini, "cli-sess-2"));
            drop(holder);
            assert!(!is_local_session_occupied(&gemini, "cli-sess-2"));
        }
    }
}
