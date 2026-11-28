use super::{CodexHarness, error, runtime::Connection};
use crate::{
    HarnessAuthorization,
    background_tasks::{bounded_tail, describe, result, terminal},
    now_ms,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};
use tokio::time::{Duration, Instant};

const OUTPUT: usize = 16 * 1024;
const MAX_TRACKED: usize = 512;

fn handle(generation: &str, thread: &str, task: &str, process: &str) -> String {
    // 锁定会话、条目、执行进程和连接代次；不信任前端提交的 PID，也不把代次外的同名任务当作原任务。
    let bytes = serde_json::to_vec(&(generation, thread, task, process)).expect("string tuple");
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Default)]
pub(super) struct Registry {
    generation: String,
    items: BTreeMap<(String, String), Value>,
    reconciled: BTreeMap<String, Instant>,
    unsupported_list: Option<String>,
    unsupported_stop: Option<String>,
}

impl Registry {
    fn entry(&mut self, generation: &str, thread: &str, id: &str) -> &mut Value {
        if self.generation != generation {
            self.items.clear();
            self.reconciled.clear();
            self.generation = generation.into();
        }
        let key = (thread.into(), id.into());
        if self.items.len() >= MAX_TRACKED
            && !self.items.contains_key(&key)
            && let Some(old) = self
                .items
                .iter()
                .min_by_key(|(_, r)| {
                    (
                        r["confirmed"] == true && terminal(&r["status"]).is_none(),
                        r["observedAtMs"].as_u64(),
                    )
                })
                .map(|(k, _)| k.clone())
        {
            self.items.remove(&old);
        }
        self.items.entry(key).or_insert_with(|| {
            let mut row = describe(id, "command", &Value::Null);
            row["observedAtMs"] = json!(now_ms());
            row
        })
    }

    fn get(&self, generation: &str, thread: &str, id: &str) -> Option<&Value> {
        (self.generation == generation)
            .then(|| self.items.get(&(thread.into(), id.into())))
            .flatten()
    }
}

fn presented(row: &Value, live: bool) -> Value {
    let mut r = row.clone();
    let completed = terminal(&r["status"]).is_some();
    if !completed {
        r["status"] = json!(if live { "running" } else { "unknown" });
    }
    // Codex 在 Windows 主动终止的实测结果是 failed/-1。保留原生状态和退出码；
    // 只有该任务确实被原生终止且收到对应结束条目时才显示已停止，自然完成或真实构建错误优先。
    if r["terminated"] == true && r["status"] == "failed" && r["exitCode"] == -1 {
        r["nativeStatus"] = json!("failed");
        r["status"] = json!("stopped");
    }
    if let Some(o) = r.as_object_mut() {
        for field in ["pid", "confirmed", "observedAtMs", "verifiedAt"] {
            o.remove(field);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_task_late_stop_does_not_replace_the_native_exit() {
        for (status, code, stopped, expected) in [
            ("completed", 0, true, "completed"),
            ("failed", 5, true, "failed"),
            ("failed", -1, false, "failed"),
            ("failed", -1, true, "stopped"),
            ("unknown", 0, true, "unknown"),
        ] {
            let row = json!({"id":"A","status":status,"exitCode":code,"terminated":stopped});
            assert_eq!(presented(&row, false)["status"], expected);
        }
        assert_ne!(
            handle("g", "threadA", "A", "42"),
            handle("g", "threadB", "A", "42")
        );
        assert_ne!(
            handle("g", "threadA", "A", "42"),
            handle("other", "threadA", "A", "42")
        );
        assert_eq!(bounded_tail("a中文字", 4), ("字", true));
        assert!(!crate::background_tasks::supported_since(
            "unknown",
            "0.167.0-alpha.4.1"
        ));
        assert!(!crate::background_tasks::supported_since(
            "codex_cli_rs/0.144.1",
            "0.167.0-alpha.4.1"
        ));
    }
}

impl CodexHarness {
    async fn save_task_result(&self, thread: &str, row: &Value) -> anyhow::Result<()> {
        let mut value = presented(row, false);
        value["canStop"] = json!(false);
        if let Some(o) = value.as_object_mut() {
            o.remove("token");
            if o.get("outputKind").is_some_and(|v| v == "live") {
                o.remove("outputKind");
            }
        }
        let mut store = self.store.lock().await;
        if store.data.deletions.contains_key(thread) {
            return Ok(());
        }
        let existing = store
            .data
            .task_records
            .get(thread)
            .and_then(|rows| rows.iter().find(|r| r["id"] == value["id"]));
        if existing.is_some_and(|old| {
            old == &value
                || terminal(&old["status"]).is_some() && terminal(&value["status"]).is_none()
                || old["status"] == "stopped"
                    && value["status"] == "failed"
                    && old["exitCode"] == value["exitCode"]
        }) {
            return Ok(());
        }
        store.commit(|data| {
            let rows = data.task_records.entry(thread.into()).or_default();
            rows.retain(|r| r["id"] != value["id"]);
            rows.push(value);
            // 已确认身份最多存 512 条，每项仅保留输出末尾 16 KiB，不挤占会话队列的 64 MiB 上限。
            while data.task_records.values().map(Vec::len).sum::<usize>() > MAX_TRACKED {
                let key = data
                    .task_records
                    .iter()
                    .filter_map(|(k, v)| {
                        v.first().map(|r| {
                            (
                                k.clone(),
                                r["completedAtMs"]
                                    .as_u64()
                                    .or(r["startedAtMs"].as_u64())
                                    .unwrap_or(0),
                            )
                        })
                    })
                    .min_by_key(|x| x.1)
                    .map(|x| x.0);
                let Some(k) = key else { break };
                let rows = data.task_records.get_mut(&k).unwrap();
                rows.remove(0);
                if rows.is_empty() {
                    data.task_records.remove(&k);
                }
            }
            Ok(())
        })
    }

    pub(super) async fn observe_task_event(
        self: &Arc<Self>,
        generation: &str,
        thread: &str,
        method: &str,
        p: &Value,
    ) {
        let item = &p["item"];
        let is_delta = method == "item/commandExecution/outputDelta";
        if !is_delta
            && !(matches!(method, "item/started" | "item/completed")
                && item["type"] == "commandExecution"
                && item["source"] == "unifiedExecStartup"
                && item["processId"].is_string())
        {
            return;
        }
        let Some(id) = (if is_delta { &p["itemId"] } else { &item["id"] }).as_str() else {
            return;
        };
        let mut reg = self.task_registry.lock().await;
        if is_delta && reg.get(generation, thread, id).is_none() {
            return;
        }
        let row = reg.entry(generation, thread, id);
        if p["turnId"].is_string() {
            row["turnId"] = p["turnId"].clone();
        }
        if is_delta {
            if terminal(&row["status"]).is_some() {
                return;
            }
            if let Some(s) = p["delta"].as_str() {
                let mut text = row["output"].as_str().unwrap_or_default().to_owned();
                text.push_str(s);
                let (tail, cut) = bounded_tail(&text, OUTPUT);
                row["output"] = json!(tail);
                if cut {
                    row["outputTruncated"] = json!(true);
                }
                row["outputKind"] = json!("live");
            }
            return;
        }
        row["pid"] = item["processId"].clone();
        row["command"] = item["command"].clone();
        if row["name"].is_null() {
            row["name"] = item["command"].clone();
        }
        if item["startedAtMs"].is_u64() {
            row["startedAtMs"] = item["startedAtMs"].clone();
        }
        if method == "item/completed" {
            if let Some(state) = terminal(&item["status"]) {
                row["status"] = json!(state);
                row["exitCode"] = item["exitCode"].clone();
                row["elapsedMs"] = item["durationMs"].clone();
                row["completedAtMs"] = item["completedAtMs"].clone();
                if let Some(output) = item["aggregatedOutput"].as_str() {
                    let (tail, cut) = bounded_tail(output, OUTPUT);
                    row["output"] = json!(tail);
                    row["outputTruncated"] = json!(cut);
                    row["outputKind"] = json!("final");
                }
                let mut completed = row.clone();
                drop(reg);
                // 重连后开始事件不会重放；只接管曾被原生列表证实过、会话/条目/进程均匹配的结束。
                if completed["confirmed"] != true {
                    let store = self.store.lock().await;
                    let Some(known) = store
                        .data
                        .task_records
                        .get(thread)
                        .into_iter()
                        .flatten()
                        .find(|r| {
                            r["id"] == id
                                && r["status"] == "unknown"
                                && r["nativeProcessId"] == completed["pid"]
                        })
                    else {
                        return;
                    };
                    completed["confirmed"] = json!(true);
                    completed["terminated"] = known["terminated"].clone();
                    completed["nativeProcessId"] = completed["pid"].clone();
                }
                if let Err(e) = self.save_task_result(thread, &completed).await {
                    let _ = crate::diagnostic_log::append_diagnostic_log(
                        "background_task.record_failed",
                        json!({"threadId":thread,"error":e.to_string()}),
                    );
                }
            }
            return;
        }
        drop(reg);
        // 即使网页没打开菜单，也要在命令仍存活时用原生列表确认它是否转后台；普通前台命令不写记录。
        {
            let p = self.projection.lock().await;
            if p.sides.threads.contains_key(thread)
                || p.threads
                    .get(thread)
                    .is_some_and(|t| t["ephemeral"] == true)
            {
                return;
            }
        }
        let harness = self.clone();
        let thread = thread.to_owned();
        let generation = generation.to_owned();
        tokio::spawn(async move {
            if let Some(c) = harness
                .runtime
                .existing_task_connection()
                .await
                .filter(|c| c.generation == generation)
            {
                let _ = harness.list_background(&c, &thread).await;
            }
        });
    }

    async fn list_background(
        &self,
        connection: &Connection,
        thread: &str,
    ) -> anyhow::Result<Vec<Value>> {
        if self.task_registry.lock().await.unsupported_list.as_deref()
            == Some(connection.generation.as_str())
        {
            return Err(error(409, "此执行引擎未提供后台终端列表"));
        }
        let mut cursor = Value::Null;
        let mut cursors = HashSet::new();
        let mut ids = HashSet::new();
        let mut pids = HashSet::new();
        let mut tasks = Vec::new();
        loop {
            let page = match connection
                .request(
                    "thread/backgroundTerminals/list",
                    json!({"threadId":thread,"limit":100,"cursor":cursor}),
                    None,
                )
                .await
            {
                Err(e)
                    if e.downcast_ref::<super::runtime::UnsupportedMethod>()
                        .is_some() =>
                {
                    self.task_registry.lock().await.unsupported_list =
                        Some(connection.generation.clone());
                    return Err(e);
                }
                other => other?,
            };
            let data = page["data"]
                .as_array()
                .ok_or_else(|| error(503, "后台终端列表格式无效"))?;
            for item in data {
                let id = item["itemId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| error(503, "后台终端缺少条目标识"))?;
                let pid = item["processId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| error(503, "后台终端缺少运行标识"))?;
                if !ids.insert(id.to_owned())
                    || !pids.insert(pid.to_owned())
                    || tasks.len() >= MAX_TRACKED
                {
                    return Err(error(503, "后台终端列表重复或超出读取范围"));
                }
                tasks.push(item.clone());
            }
            match &page["nextCursor"] {
                Value::Null => break,
                Value::String(s)
                    if !s.is_empty() && cursors.insert(s.clone()) && cursors.len() < 32 =>
                {
                    cursor = json!(s)
                }
                _ => return Err(error(503, "后台终端分页不完整")),
            }
        }
        let current = self.runtime.existing_task_connection().await;
        if current
            .as_ref()
            .is_none_or(|c| c.generation != connection.generation)
        {
            return Err(error(409, "后台任务连接已变化"));
        }
        let mut out = Vec::new();
        let mut save = Vec::new();
        {
            let mut reg = self.task_registry.lock().await;
            let stop_supported = reg.unsupported_stop.as_deref()
                != Some(connection.generation.as_str())
                && connection.user_agent.get().is_some_and(|v| {
                    crate::background_tasks::supported_since(v, "0.167.0-alpha.4.1")
                });
            if reg.generation == connection.generation {
                for ((session, id), row) in &mut reg.items {
                    if session == thread && !ids.contains(id) {
                        row["verifiedAt"] = Value::Null;
                    }
                }
            }
            for item in tasks {
                let id = item["itemId"].as_str().unwrap();
                let pid = item["processId"].as_str().unwrap();
                let r = reg.entry(&connection.generation, thread, id);
                if r["pid"].is_string() && r["pid"] != pid {
                    continue;
                }
                let first = r["confirmed"] != true;
                r["confirmed"] = json!(true);
                r["pid"] = json!(pid);
                r["nativeProcessId"] = json!(pid);
                r["name"] = item["command"].clone();
                if r["command"].is_null() {
                    r["command"] = item["command"].clone();
                }
                r["token"] = json!(handle(&connection.generation, thread, id, pid));
                r["verifiedAt"] = json!(now_ms());
                if first {
                    save.push(r.clone());
                }
                if terminal(&r["status"]).is_some() {
                    continue;
                }
                let mut active = presented(r, true);
                active["canStop"] = json!(r["terminated"] != true && stop_supported);
                if r["terminated"] == true {
                    active["stopRequested"] = json!(true);
                }
                if let Some(start) = r["startedAtMs"].as_u64() {
                    active["elapsedMs"] = json!(now_ms().saturating_sub(start));
                }
                out.push(active);
            }
        }
        for r in save {
            self.save_task_result(thread, &r).await?;
        }
        Ok(out)
    }

    /// 原先观察到的后台终端可能在 Harness 离线时结束。只回查其已知身份；不把别的历史命令加入记录。
    async fn reconcile_task_history(
        &self,
        c: &Connection,
        thread: &str,
        saved: &[Value],
        detail: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut pending: BTreeMap<_, _> = saved
            .iter()
            .filter(|r| r["status"] == "unknown" && detail.is_none_or(|id| r["id"] == id))
            .filter_map(|r| Some((r["id"].as_str()?, r)))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        {
            let mut reg = self.task_registry.lock().await;
            if reg.generation == c.generation {
                pending.retain(|id, _| {
                    reg.get(&c.generation, thread, id).is_none_or(|r| {
                        r["verifiedAt"]
                            .as_u64()
                            .is_none_or(|sample| now_ms().saturating_sub(sample) > 7000)
                    })
                });
            }
            if pending.is_empty() {
                return Ok(());
            }
            if reg
                .reconciled
                .get(thread)
                .is_some_and(|seen| seen.elapsed() < Duration::from_secs(15))
            {
                return Ok(());
            }
            if reg.reconciled.len() >= 512 {
                reg.reconciled.clear();
            }
            reg.reconciled.insert(thread.to_owned(), Instant::now());
        }
        let mut cursor = Value::Null;
        let mut seen = HashSet::new();
        for _ in 0..16 {
            let page = c
                .request(
                    "thread/items/list",
                    json!({"threadId":thread,"sortDirection":"desc","limit":100,"cursor":cursor}),
                    None,
                )
                .await?;
            let entries = page["data"]
                .as_array()
                .ok_or_else(|| error(503, "任务历史响应格式无效"))?;
            for entry in entries {
                let item = entry.get("item").unwrap_or(entry);
                let Some((id, old)) = item["id"]
                    .as_str()
                    .and_then(|id| Some((id, *pending.get(id)?)))
                else {
                    continue;
                };
                if entry["threadId"].as_str().is_some_and(|s| s != thread)
                    || item["type"] != "commandExecution"
                    || old["turnId"].is_string() && entry["turnId"] != old["turnId"]
                    || item["processId"].is_string() && item["processId"] != old["nativeProcessId"]
                {
                    continue;
                }
                let Some(state) = terminal(&item["status"]) else {
                    continue;
                };
                let mut row = old.clone();
                row["status"] = json!(state);
                for (src, dst) in [
                    ("durationMs", "elapsedMs"),
                    ("exitCode", "exitCode"),
                    ("startedAtMs", "startedAtMs"),
                    ("completedAtMs", "completedAtMs"),
                ] {
                    if let Some(num) = item[src].as_i64().or(entry[src].as_i64()) {
                        row[dst] = json!(num);
                    }
                }
                if let Some(raw) = item["aggregatedOutput"].as_str() {
                    let (s, cut) = bounded_tail(raw, OUTPUT);
                    row["output"] = json!(s);
                    row["outputTruncated"] = json!(cut);
                }
                self.save_task_result(thread, &row).await?;
                pending.remove(id);
            }
            if pending.is_empty() || page["nextCursor"].is_null() {
                return Ok(());
            }
            let next = page["nextCursor"]
                .as_str()
                .filter(|s| !s.is_empty() && seen.insert(s.to_string()))
                .ok_or_else(|| error(503, "任务历史分页无效"))?;
            cursor = json!(next);
        }
        Err(error(503, "较早的后台任务结束状态尚未核实"))
    }

    pub(super) async fn read_background_tasks(
        &self,
        action: &str,
        payload: &Value,
    ) -> anyhow::Result<Value> {
        let thread = super::required(payload, "threadId")?;
        let detail = payload["taskId"].as_str();
        if self.known_side(thread).await {
            return Err(error(409, "临时侧边会话不保留后台任务记录"));
        }
        let mut saved = if action == "backgroundTasks" {
            Vec::new()
        } else {
            self.store
                .lock()
                .await
                .data
                .task_records
                .get(thread)
                .cloned()
                .unwrap_or_default()
        };
        let connection = self.runtime.existing_task_connection().await;
        let mut reason = None;
        if action != "backgroundTasks"
            && saved
                .iter()
                .any(|r| r["status"] == "unknown" && detail.is_none_or(|id| r["id"] == id))
            && let Some(c) = &connection
        {
            if let Err(e) = self.reconcile_task_history(c, thread, &saved, detail).await {
                reason = Some(e.to_string());
            }
            saved = self
                .store
                .lock()
                .await
                .data
                .task_records
                .get(thread)
                .cloned()
                .unwrap_or_default();
        }
        if action == "backgroundTaskRecords" {
            let available = !saved.is_empty();
            let rows = saved
                .into_iter()
                .map(|mut r| {
                    if let Some(o) = r.as_object_mut() {
                        o.remove("output");
                        o.remove("command");
                    }
                    r
                })
                .collect();
            return Ok(result(
                thread,
                rows,
                None,
                reason
                    .as_deref()
                    .or((!available).then_some("暂无经原生后台终端列表确认的结束记录")),
            ));
        }
        if let Some(id) = detail
            && let Some(r) = saved
                .iter()
                .find(|r| r["id"] == id && terminal(&r["status"]).is_some())
        {
            return Ok(result(thread, vec![r.clone()], detail, None));
        }
        let active = if let Some(c) = connection {
            match self.list_background(&c, thread).await {
                Ok(list) => Some(list),
                Err(e) => {
                    reason = Some(e.to_string());
                    None
                }
            }
        } else {
            reason = Some("当前执行引擎未连接，无法核对运行状态".into());
            None
        };
        if let Some(id) = detail {
            if let Some(r) = active.iter().flatten().find(|r| r["id"] == id) {
                return Ok(result(thread, vec![r.clone()], detail, None));
            }
            let mut saved = self
                .store
                .lock()
                .await
                .data
                .task_records
                .get(thread)
                .cloned()
                .unwrap_or_default();
            // 连接窗口内终态可能刚写入；绝不以任务列表缺项冒充停止/完成。
            saved.retain(|r| r["id"] == id);
            return Ok(result(thread, saved, detail, reason.as_deref()));
        }
        let items = active.map(|rows| {
            rows.into_iter()
                .map(|r| {
                    json!({"id":r["id"],"kind":"shell","name":r["name"],
            "status":"running","elapsedMs":r["elapsedMs"]})
                })
                .collect::<Vec<_>>()
        });
        Ok(json!({"ok":true,"threadId":thread,"items":items,"reason":reason}))
    }

    pub(super) async fn stop_background_task(
        &self,
        thread: &str,
        payload: &Value,
        auth: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let c = self
            .runtime
            .existing_task_connection()
            .await
            .ok_or_else(|| error(409, "当前引擎未连接"))?;
        // 即使客户端自行构造请求，也不允许借只读子代理或过期临时侧边去控制其他会话。
        let identity = c
            .request(
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
                None,
            )
            .await?;
        if identity["thread"]["id"] != thread
            || crate::codex_service::subagent_parent_id(&identity["thread"]).is_some()
            || identity["thread"]["ephemeral"] == true && self.known_side(thread).await
        {
            return Err(error(403, "此会话不提供后台任务写操作"));
        }
        crate::codex_service::normalize_session_response(&identity)?;
        if crate::codex_service::attached_parent_id(&identity["thread"]).is_some() {
            self.persistent_snapshot(&identity["thread"]).await?;
        }
        let owned = self.loaded.lock().await.get(thread).cloned();
        if self
            .runtime
            .external_thread_writer(thread, owned.as_deref())
            .await?
        {
            return Err(error(409, "会话已由其他应用持有"));
        }
        let tasks = self.list_background(&c, thread).await?;
        let r = tasks
            .iter()
            .find(|r| {
                r["id"] == payload["taskId"]
                    && r["token"] == payload["token"]
                    && r["canStop"] == true
            })
            .ok_or_else(|| error(409, "任务已结束或运行身份已变化"))?;
        let pid = r["nativeProcessId"]
            .as_str()
            .ok_or_else(|| error(409, "缺少已核实的后台进程标识"))?;
        // 不接受页面指定 PID，也不使用 clean、turn/interrupt 或系统进程终止；只在同一连接执行一次。
        auth.check().await?;
        let reply = match c
            .request(
                "thread/backgroundTerminals/terminate",
                json!({"threadId":thread,"processId":pid}),
                Some(auth),
            )
            .await
        {
            Err(e)
                if e.downcast_ref::<super::runtime::UnsupportedMethod>()
                    .is_some() =>
            {
                self.task_registry.lock().await.unsupported_stop = Some(c.generation.clone());
                return Err(e);
            }
            other => other?,
        };
        if reply["terminated"] != true {
            return Err(error(409, "原生引擎未确认终止该后台任务"));
        }
        let task = payload["taskId"].as_str().unwrap();
        let save = {
            let mut reg = self.task_registry.lock().await;
            if reg.generation != c.generation {
                None
            } else {
                let r = reg.entry(&c.generation, thread, task);
                r["terminated"] = json!(true);
                Some(r.clone())
            }
        };
        if let Some(r) = save {
            self.save_task_result(thread, &r).await?;
        }
        Ok(json!({"ok":true,"threadId":thread,"taskId":task,"status":"requested"}))
    }
}
