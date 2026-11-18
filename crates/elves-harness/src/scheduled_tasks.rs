//! 本机 Harness 的定时任务存储与调度。
//!
//! 定时任务必须保存明确的实例和模型，执行时复用现有会话创建入口。
//! 这样 Codex 与 Claude 共用同一套 UI 和任务生命周期，同时仍由各自实例
//! 的原生目录、权限和执行适配器负责实际派发。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::{Datelike, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{ApiError, HarnessAuthorization, HarnessController};

const MAX_TASKS: usize = 256;
const MAX_NAME_BYTES: usize = 160;
const MAX_PROMPT_BYTES: usize = 128 * 1024;
const MAX_CRON_BYTES: usize = 128;
const MAX_SCHEDULE_MINUTES: u64 = 366 * 24 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScheduledTask {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) prompt: String,
    pub(crate) backend: String,
    pub(crate) instance_id: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) reasoning_effort: String,
    #[serde(default)]
    pub(crate) project_id: Option<String>,
    pub(crate) schedule: Schedule,
    pub(crate) enabled: bool,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) next_run_at_ms: Option<u64>,
    #[serde(default)]
    pub(crate) last_run_at_ms: Option<u64>,
    #[serde(default)]
    pub(crate) last_run_status: Option<String>,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    #[serde(default)]
    pub(crate) last_thread_id: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Schedule {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) start_at_ms: Option<u64>,
    #[serde(default)]
    pub(crate) interval_minutes: Option<u64>,
    #[serde(default)]
    pub(crate) cron: Option<String>,
    #[serde(default)]
    pub(crate) timezone: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    revision: u64,
    tasks: Vec<ScheduledTask>,
}

struct State {
    stored: Stored,
    load_failed: bool,
}

pub(crate) struct ScheduledTasks {
    path: PathBuf,
    state: Mutex<State>,
}

impl ScheduledTasks {
    pub(crate) fn new(directory: &Path) -> anyhow::Result<Self> {
        let path = directory.join("harness-scheduled-tasks.json");
        let (stored, load_failed) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
                Ok(mut stored) => {
                    stored.tasks.retain(|task| valid_task_id(&task.id));
                    (stored, false)
                }
                Err(error) => {
                    Self::log_failure(&error);
                    (Stored::default(), true)
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (Stored::default(), false)
            }
            Err(error) => {
                Self::log_failure(&error);
                (Stored::default(), true)
            }
        };
        Ok(Self {
            path,
            state: Mutex::new(State {
                stored,
                load_failed,
            }),
        })
    }

    fn log_failure(error: impl std::fmt::Display) {
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "harness.scheduled_tasks_storage_failed",
            json!({"error":format!("{error:#}")}),
        );
    }

    pub(crate) async fn snapshot(&self) -> Result<Value, ApiError> {
        let state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时任务存储暂不可用，请重启 Harness 后重试",
            ));
        }
        Ok(json!({
            "ok":true,
            "revision":state.stored.revision,
            "tasks":sorted_tasks(&state.stored.tasks),
        }))
    }

    pub(crate) async fn action(
        &self,
        controller: &HarnessController,
        action: &str,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        match action {
            "createScheduledTask" => {
                let now = super::now_ms();
                let task = task_from_payload(payload.get("task").unwrap_or(payload), None, now)?;
                self.validate_target(controller, &task, authorization)
                    .await?;
                let task = self.insert(task).await?;
                controller.publish_event(json!({
                    "method":"remote/scheduledTasksChanged",
                    "params":{"taskId":task.id,"reason":"created"}
                }));
                Ok(json!({"ok":true,"task":task}))
            }
            "updateScheduledTask" => {
                let task_id = task_id(payload)?;
                let now = super::now_ms();
                let task = {
                    let state = self.state.lock().await;
                    if state.load_failed {
                        return Err(ApiError::new(
                            503,
                            "定时任务存储暂不可用，请重启 Harness 后重试",
                        )
                        .into());
                    }
                    let existing = state
                        .stored
                        .tasks
                        .iter()
                        .find(|task| task.id == task_id)
                        .cloned()
                        .ok_or_else(|| ApiError::new(404, "定时任务不存在"))?;
                    task_from_payload(payload.get("task").unwrap_or(payload), Some(&existing), now)?
                };
                self.validate_target(controller, &task, authorization)
                    .await?;
                self.replace(task.clone()).await?;
                controller.publish_event(json!({
                    "method":"remote/scheduledTasksChanged",
                    "params":{"taskId":task.id,"reason":"updated"}
                }));
                Ok(json!({"ok":true,"task":task}))
            }
            "deleteScheduledTask" => {
                let task_id = task_id(payload)?;
                self.remove(&task_id).await?;
                controller.publish_event(json!({
                    "method":"remote/scheduledTasksChanged",
                    "params":{"taskId":task_id,"reason":"deleted"}
                }));
                Ok(json!({"ok":true,"taskId":task_id}))
            }
            "toggleScheduledTask" => {
                let task_id = task_id(payload)?;
                let enabled = payload["enabled"]
                    .as_bool()
                    .ok_or_else(|| ApiError::new(400, "定时任务启用状态无效"))?;
                let task = self.toggle(&task_id, enabled, super::now_ms()).await?;
                controller.publish_event(json!({
                    "method":"remote/scheduledTasksChanged",
                    "params":{"taskId":task.id,"reason":"toggled"}
                }));
                Ok(json!({"ok":true,"task":task}))
            }
            "runScheduledTask" => {
                let task_id = task_id(payload)?;
                let task = self.begin_manual(&task_id, super::now_ms()).await?;
                match self
                    .run_one(controller, task, authorization, "manual")
                    .await
                {
                    Ok(result) => {
                        controller.publish_event(json!({
                            "method":"remote/scheduledTasksChanged",
                            "params":{"taskId":task_id,"reason":"run"}
                        }));
                        Ok(json!({"ok":true,"taskId":task_id,"result":result}))
                    }
                    Err(error) => {
                        self.finish_error(&task_id, &error.to_string()).await;
                        controller.publish_event(json!({
                            "method":"remote/scheduledTasksChanged",
                            "params":{"taskId":task_id,"reason":"failed"}
                        }));
                        Err(error)
                    }
                }
            }
            _ => Err(ApiError::new(404, "不支持的定时任务操作").into()),
        }
    }

    pub(crate) async fn tick(self: &Arc<Self>, controller: &Arc<HarnessController>) {
        if controller
            .closing
            .load(std::sync::atomic::Ordering::Acquire)
            || !controller
                .desktop_running
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let due = self.claim_due(super::now_ms()).await;
        for task in due {
            let device = controller.desktop_device.lock().await.clone();
            let Some(device) = device else {
                self.finish_error(&task.id, "Elves 本机授权尚未就绪").await;
                continue;
            };
            let authorization = controller.authorization(device, Some("runScheduledTask"));
            if let Err(error) = self
                .run_one(controller, task.clone(), &authorization, "scheduled")
                .await
            {
                self.finish_error(&task.id, &error.to_string()).await;
            }
            controller.publish_event(json!({
                "method":"remote/scheduledTasksChanged",
                "params":{"taskId":task.id,"reason":"completed"}
            }));
        }
    }

    async fn insert(&self, mut task: ScheduledTask) -> Result<ScheduledTask, ApiError> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时任务存储暂不可用，请重启 Harness 后重试",
            ));
        }
        if state.stored.tasks.len() >= MAX_TASKS {
            return Err(ApiError::new(409, "定时任务数量已达上限"));
        }
        task.created_at_ms = super::now_ms();
        task.updated_at_ms = task.created_at_ms;
        save_locked(&self.path, &mut state, |stored| {
            stored.tasks.push(task.clone());
            Ok(())
        })?;
        Ok(task)
    }

    async fn validate_target(
        &self,
        controller: &HarnessController,
        task: &ScheduledTask,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<()> {
        let mut payload = json!({
            "backend":task.backend,
            "instanceId":task.instance_id,
            "includeAvailable":true
        });
        if let Some(project_id) = task.project_id.as_deref() {
            payload["projectId"] = json!(project_id);
        }
        let catalog = controller
            .call_backend("models", payload, authorization)
            .await?;
        let found = catalog["data"].as_array().is_some_and(|rows| {
            rows.iter().any(|row| {
                row["id"] == task.model
                    || row["model"] == task.model
                    || row["requestModel"] == task.model
            })
        });
        if !found {
            return Err(ApiError::new(409, "所选模型不属于当前实例，已拒绝保存定时任务").into());
        }
        Ok(())
    }

    async fn replace(&self, task: ScheduledTask) -> Result<(), ApiError> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时任务存储暂不可用，请重启 Harness 后重试",
            ));
        }
        if !state
            .stored
            .tasks
            .iter()
            .any(|current| current.id == task.id)
        {
            return Err(ApiError::new(404, "定时任务不存在"));
        }
        save_locked(&self.path, &mut state, |stored| {
            let current = stored
                .tasks
                .iter_mut()
                .find(|current| current.id == task.id)
                .context("定时任务不存在")?;
            *current = task.clone();
            Ok(())
        })
    }

    async fn remove(&self, id: &str) -> Result<(), ApiError> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时任务存储暂不可用，请重启 Harness 后重试",
            ));
        }
        if !state.stored.tasks.iter().any(|task| task.id == id) {
            return Err(ApiError::new(404, "定时任务不存在"));
        }
        save_locked(&self.path, &mut state, |stored| {
            stored.tasks.retain(|task| task.id != id);
            Ok(())
        })
    }

    async fn begin_manual(&self, id: &str, now: u64) -> Result<ScheduledTask, ApiError> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时任务存储暂不可用，请重启 Harness 后重试",
            ));
        }
        let mut task = state
            .stored
            .tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
            .ok_or_else(|| ApiError::new(404, "定时任务不存在"))?;
        if task.status == "running" {
            return Err(ApiError::new(409, "定时任务正在执行，请等待本次运行结束"));
        }
        // 手动运行也先持久化 running，避免连续点击或调度线程并发创建多个会话。
        task.status = "running".into();
        task.updated_at_ms = now;
        save_locked(&self.path, &mut state, |stored| {
            let current = stored
                .tasks
                .iter_mut()
                .find(|current| current.id == id)
                .context("定时任务不存在")?;
            *current = task.clone();
            Ok(())
        })?;
        Ok(task)
    }

    async fn toggle(&self, id: &str, enabled: bool, now: u64) -> Result<ScheduledTask, ApiError> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时任务存储暂不可用，请重启 Harness 后重试",
            ));
        }
        let mut updated = state
            .stored
            .tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
            .ok_or_else(|| ApiError::new(404, "定时任务不存在"))?;
        updated.enabled = enabled;
        updated.status = if enabled { "active" } else { "paused" }.into();
        updated.next_run_at_ms = enabled
            .then(|| next_run_at(&updated.schedule, now))
            .transpose()
            .map_err(|error| ApiError::new(400, error.to_string()))?
            .flatten();
        updated.updated_at_ms = now;
        save_locked(&self.path, &mut state, |stored| {
            let current = stored
                .tasks
                .iter_mut()
                .find(|current| current.id == id)
                .context("定时任务不存在")?;
            *current = updated.clone();
            Ok(())
        })?;
        Ok(updated)
    }

    async fn claim_due(&self, now: u64) -> Vec<ScheduledTask> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Vec::new();
        }
        let mut due = Vec::new();
        for task in &mut state.stored.tasks {
            if !task.enabled
                || !matches!(task.status.as_str(), "active" | "error")
                || task.next_run_at_ms.is_none_or(|at| at > now)
            {
                continue;
            }
            due.push(task.clone());
            task.status = "running".into();
            task.updated_at_ms = now;
            task.next_run_at_ms = if task.schedule.kind == "once" {
                None
            } else {
                next_run_at(&task.schedule, now).ok().flatten()
            };
        }
        if !due.is_empty() {
            if let Err(error) = save_locked(&self.path, &mut state, |_| Ok(())) {
                Self::log_failure(&anyhow::anyhow!("{error}"));
            }
        }
        due
    }

    async fn run_one(
        &self,
        controller: &HarnessController,
        task: ScheduledTask,
        authorization: &HarnessAuthorization,
        source: &str,
    ) -> anyhow::Result<Value> {
        authorization.check().await?;
        let mut payload = json!({
            "backend":task.backend,
            "instanceId":task.instance_id,
            "model":task.model,
            "prompt":task.prompt,
        });
        if !task.reasoning_effort.is_empty() {
            payload["reasoningEffort"] = json!(task.reasoning_effort);
        }
        if let Some(project_id) = task.project_id.as_deref() {
            payload["projectId"] = json!(project_id);
        }
        let result = controller
            .call_backend("create", payload, authorization)
            .await?;
        let thread_id = result["conversationId"]
            .as_str()
            .or_else(|| result["threadId"].as_str())
            .map(str::to_owned);
        let now = super::now_ms();
        self.finish_success(&task, now, thread_id, source).await?;
        Ok(result)
    }

    async fn finish_success(
        &self,
        task: &ScheduledTask,
        now: u64,
        thread_id: Option<String>,
        _source: &str,
    ) -> Result<(), ApiError> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时任务存储暂不可用，请重启 Harness 后重试",
            ));
        }
        let mut updated = state
            .stored
            .tasks
            .iter()
            .find(|current| current.id == task.id)
            .cloned()
            .ok_or_else(|| ApiError::new(404, "定时任务不存在"))?;
        updated.last_run_at_ms = Some(now);
        updated.last_run_status = Some("success".into());
        updated.last_error = None;
        updated.last_thread_id = thread_id;
        updated.updated_at_ms = now;
        if updated.schedule.kind == "once" {
            updated.enabled = false;
            updated.status = "completed".into();
            updated.next_run_at_ms = None;
        } else if updated.enabled {
            updated.status = "active".into();
            updated.next_run_at_ms = next_run_at(&updated.schedule, now)
                .map_err(|error| ApiError::new(400, error.to_string()))?;
        } else {
            updated.status = "paused".into();
            updated.next_run_at_ms = None;
        }
        save_locked(&self.path, &mut state, |stored| {
            let current = stored
                .tasks
                .iter_mut()
                .find(|current| current.id == task.id)
                .context("定时任务不存在")?;
            *current = updated.clone();
            Ok(())
        })?;
        Ok(())
    }

    async fn finish_error(&self, id: &str, message: &str) {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return;
        }
        let now = super::now_ms();
        let Some(mut updated) = state
            .stored
            .tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
        else {
            return;
        };
        updated.last_run_at_ms = Some(now);
        updated.last_run_status = Some("failed".into());
        updated.last_error = Some(message.chars().take(512).collect());
        updated.updated_at_ms = now;
        updated.status = if updated.enabled { "error" } else { "paused" }.into();
        if updated.enabled && updated.schedule.kind != "once" {
            updated.next_run_at_ms = next_run_at(&updated.schedule, now).ok().flatten();
        } else {
            updated.next_run_at_ms = None;
        }
        if let Err(error) = save_locked(&self.path, &mut state, |stored| {
            let current = stored
                .tasks
                .iter_mut()
                .find(|current| current.id == id)
                .context("定时任务不存在")?;
            *current = updated.clone();
            Ok(())
        }) {
            Self::log_failure(&anyhow::anyhow!("{error}"));
        }
    }
}

fn save_locked(
    path: &Path,
    state: &mut State,
    change: impl FnOnce(&mut Stored) -> anyhow::Result<()>,
) -> Result<(), ApiError> {
    let mut next = state.stored.clone();
    change(&mut next).map_err(|error| ApiError::new(500, error.to_string()))?;
    next.revision = next
        .revision
        .checked_add(1)
        .ok_or_else(|| ApiError::new(409, "定时任务版本已达上限"))?;
    let bytes = serde_json::to_vec(&next).map_err(|error| ApiError::new(500, error.to_string()))?;
    crate::settings::atomic_write(path, &bytes)
        .map_err(|error| ApiError::new(503, format!("无法保存定时任务：{error}")))?;
    state.stored = next;
    Ok(())
}

fn sorted_tasks(tasks: &[ScheduledTask]) -> Vec<ScheduledTask> {
    let mut tasks = tasks.to_vec();
    tasks.sort_by_key(|task| {
        (
            task.next_run_at_ms.unwrap_or(u64::MAX),
            task.created_at_ms,
            task.id.clone(),
        )
    });
    tasks
}

fn task_id(payload: &Value) -> Result<String, ApiError> {
    let id = payload["taskId"]
        .as_str()
        .filter(|id| valid_task_id(id))
        .ok_or_else(|| ApiError::new(400, "定时任务标识无效"))?;
    Ok(id.to_owned())
}

fn valid_task_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok()
}

fn task_from_payload(
    payload: &Value,
    existing: Option<&ScheduledTask>,
    now: u64,
) -> Result<ScheduledTask, ApiError> {
    let object = payload
        .as_object()
        .ok_or_else(|| ApiError::new(400, "定时任务参数无效"))?;
    let string_field = |key: &str, fallback: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .trim()
            .to_owned()
    };
    let name = string_field(
        "name",
        existing.map(|task| task.name.as_str()).unwrap_or(""),
    );
    let prompt = object
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_else(|| existing.map(|task| task.prompt.as_str()).unwrap_or(""))
        .to_owned();
    let instance_id = string_field(
        "instanceId",
        existing.map(|task| task.instance_id.as_str()).unwrap_or(""),
    );
    let model = string_field(
        "model",
        existing.map(|task| task.model.as_str()).unwrap_or(""),
    );
    let reasoning_effort = string_field(
        "reasoningEffort",
        existing
            .map(|task| task.reasoning_effort.as_str())
            .unwrap_or(""),
    );
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(ApiError::new(400, "任务名称需为 1–160 字节"));
    }
    // 定时任务提示是用户可编辑的多行文本；保留换行、回车和制表符，只拦截其他控制字符。
    if prompt.trim().is_empty()
        || prompt.len() > MAX_PROMPT_BYTES
        || prompt
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(ApiError::new(400, "执行提示需为 1–128 KiB"));
    }
    if !valid_instance_id(&instance_id) {
        return Err(ApiError::new(400, "必须先选择有效的会话实例"));
    }
    if model.is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
        return Err(ApiError::new(400, "必须先选择有效的请求模型"));
    }
    if !reasoning_effort.is_empty()
        && !matches!(
            reasoning_effort.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
        )
    {
        return Err(ApiError::new(400, "思考深度无效"));
    }
    let backend = instance_id.split(':').next().unwrap_or("");
    if object
        .get("backend")
        .and_then(Value::as_str)
        .is_some_and(|value| value != backend)
    {
        return Err(ApiError::new(409, "会话类型与实例不匹配"));
    }
    let project_id = object
        .get("projectId")
        .and_then(Value::as_str)
        .or_else(|| existing.and_then(|task| task.project_id.as_deref()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if project_id.as_deref().is_some_and(|value| {
        value.len() > 160 || !value.bytes().all(|byte| byte.is_ascii_graphic())
    }) {
        return Err(ApiError::new(400, "项目标识无效"));
    }
    let schedule_value = object
        .get("schedule")
        .cloned()
        .or_else(|| existing.map(|task| serde_json::to_value(&task.schedule).unwrap()))
        .ok_or_else(|| ApiError::new(400, "缺少执行计划"))?;
    let schedule = parse_schedule(&schedule_value, now)?;
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .or_else(|| existing.map(|task| task.enabled))
        .unwrap_or(true);
    let id = existing
        .map(|task| task.id.clone())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let next_run_at_ms = if enabled {
        next_run_at(&schedule, now).map_err(|error| ApiError::new(400, error.to_string()))?
    } else {
        None
    };
    let created_at_ms = existing.map(|task| task.created_at_ms).unwrap_or(now);
    Ok(ScheduledTask {
        id,
        name,
        prompt,
        backend: backend.to_owned(),
        instance_id,
        model,
        reasoning_effort,
        project_id,
        schedule,
        enabled,
        status: if enabled { "active" } else { "paused" }.into(),
        next_run_at_ms,
        last_run_at_ms: existing.and_then(|task| task.last_run_at_ms),
        last_run_status: existing.and_then(|task| task.last_run_status.clone()),
        last_error: None,
        last_thread_id: existing.and_then(|task| task.last_thread_id.clone()),
        created_at_ms,
        updated_at_ms: now,
    })
}

fn valid_instance_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b":_-".contains(&byte))
        && matches!(id.split(':').next().unwrap_or(""), "codex" | "claude")
}

fn parse_schedule(value: &Value, now: u64) -> Result<Schedule, ApiError> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::new(400, "执行计划无效"))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let start_at_ms = object.get("startAtMs").and_then(Value::as_u64);
    let interval_minutes = object.get("intervalMinutes").and_then(Value::as_u64);
    let cron = object
        .get("cron")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let timezone = object
        .get("timezone")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if timezone
        .as_deref()
        .is_some_and(|value| value.len() > 80 || value.chars().any(char::is_control))
    {
        return Err(ApiError::new(400, "时区标识无效"));
    }
    let schedule = Schedule {
        kind,
        start_at_ms,
        interval_minutes,
        cron,
        timezone,
    };
    match schedule.kind.as_str() {
        "once" => {
            if schedule.start_at_ms.is_none_or(|at| at <= now) {
                return Err(ApiError::new(400, "一次性任务必须选择未来时间"));
            }
        }
        "interval" => {
            let minutes = schedule.interval_minutes.unwrap_or(0);
            if !(1..=525_600).contains(&minutes) {
                return Err(ApiError::new(400, "周期必须为 1–525600 分钟"));
            }
            if schedule.start_at_ms.is_none_or(|at| at <= now) {
                return Err(ApiError::new(400, "周期任务必须选择未来的首次执行时间"));
            }
        }
        "cron" => {
            let expression = schedule.cron.as_deref().unwrap_or("");
            if expression.len() > MAX_CRON_BYTES
                || next_cron_after(expression, schedule.timezone.as_deref(), now).is_none()
            {
                return Err(ApiError::new(
                    400,
                    "Cron 表达式无效或一年内没有下一次执行时间",
                ));
            }
        }
        _ => return Err(ApiError::new(400, "执行计划类型无效")),
    }
    Ok(schedule)
}

fn next_run_at(schedule: &Schedule, now: u64) -> anyhow::Result<Option<u64>> {
    match schedule.kind.as_str() {
        "once" => Ok(schedule.start_at_ms),
        "interval" => {
            let start = schedule.start_at_ms.context("周期任务缺少首次执行时间")?;
            let minutes = schedule
                .interval_minutes
                .filter(|minutes| *minutes > 0)
                .context("周期任务间隔无效")?;
            let step = minutes
                .checked_mul(60_000)
                .context("周期任务间隔超出范围")?;
            if start > now {
                return Ok(Some(start));
            }
            let elapsed = now.saturating_sub(start);
            let jumps = elapsed / step + 1;
            Ok(Some(start.saturating_add(jumps.saturating_mul(step))))
        }
        "cron" => Ok(next_cron_after(
            schedule.cron.as_deref().unwrap_or(""),
            schedule.timezone.as_deref(),
            now,
        )),
        _ => anyhow::bail!("执行计划类型无效"),
    }
}

fn parse_timezone(value: Option<&str>) -> anyhow::Result<Tz> {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("UTC")
        .parse::<Tz>()
        .map_err(|_| anyhow::anyhow!("时区标识无效"))
}

fn next_cron_after(expression: &str, timezone: Option<&str>, now: u64) -> Option<u64> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        return None;
    }
    let timezone = parse_timezone(timezone).ok()?;
    let minute = parse_cron_field(fields[0], 0, 59).ok()?;
    let hour = parse_cron_field(fields[1], 0, 23).ok()?;
    let day = parse_cron_field(fields[2], 1, 31).ok()?;
    let month = parse_cron_field(fields[3], 1, 12).ok()?;
    let weekday = parse_cron_field(fields[4], 0, 6).ok()?;
    let day_restricted = !cron_field_is_wildcard(fields[2]);
    let weekday_restricted = !cron_field_is_wildcard(fields[4]);
    // 按保存的 IANA 时区解释 Cron 字段，再在 UTC 分钟上扫描，覆盖夏令时跳变和重复时刻。
    let mut cursor = (now / 60_000 + 1).saturating_mul(60_000);
    for _ in 0..MAX_SCHEDULE_MINUTES {
        let timestamp = Utc
            .timestamp_millis_opt(i64::try_from(cursor).ok()?)
            .single()?;
        let local = timestamp.with_timezone(&timezone);
        let day_matches = day.contains(&local.day());
        let weekday_matches = weekday.contains(&local.weekday().num_days_from_sunday());
        // 标准五段 Cron 在日期和星期都受限时采用 OR；其中一项为 * 时只看另一项。
        let calendar_matches = match (day_restricted, weekday_restricted) {
            (false, false) => true,
            (true, false) => day_matches,
            (false, true) => weekday_matches,
            (true, true) => day_matches || weekday_matches,
        };
        if minute.contains(&local.minute())
            && hour.contains(&local.hour())
            && month.contains(&local.month())
            && calendar_matches
        {
            return Some(cursor);
        }
        cursor = cursor.saturating_add(60_000);
    }
    None
}

fn cron_field_is_wildcard(value: &str) -> bool {
    value
        .split(',')
        .all(|part| part == "*" || part.starts_with("*/"))
}

fn parse_cron_field(value: &str, min: u32, max: u32) -> anyhow::Result<HashSet<u32>> {
    anyhow::ensure!(!value.is_empty(), "Cron 字段不能为空");
    let mut values = HashSet::new();
    for part in value.split(',') {
        let (base, step) = match part.split_once('/') {
            Some((base, step)) => (base, step.parse::<u32>().context("Cron 步长无效")?),
            None => (part, 1),
        };
        anyhow::ensure!(step > 0, "Cron 步长必须大于 0");
        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((left, right)) = base.split_once('-') {
            (left.parse::<u32>()?, right.parse::<u32>()?)
        } else {
            let single = base.parse::<u32>()?;
            if part.contains('/') {
                (single, max)
            } else {
                (single, single)
            }
        };
        anyhow::ensure!(start >= min && end <= max && start <= end, "Cron 范围无效");
        let mut current = start;
        while current <= end {
            values.insert(current);
            current = current.saturating_add(step);
            if current == 0 {
                break;
            }
        }
    }
    anyhow::ensure!(!values.is_empty(), "Cron 字段为空");
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Asia::Shanghai;

    #[test]
    fn multiline_prompt_is_valid_but_other_controls_are_rejected() {
        let now = 1_800_000_000_000;
        let task = task_from_payload(
            &json!({
                "name":"多行任务",
                "prompt":"第一行\n第二行\t继续",
                "instanceId":"codex:local",
                "model":"gpt-6.1-sol",
                "schedule":{"kind":"once","startAtMs":now + 60_000}
            }),
            None,
            now,
        )
        .unwrap();
        assert_eq!(task.prompt, "第一行\n第二行\t继续");

        let error = task_from_payload(
            &json!({
                "name":"非法任务",
                "prompt":"包含\u{0000}控制字符",
                "instanceId":"codex:local",
                "model":"gpt-6.1-sol",
                "schedule":{"kind":"once","startAtMs":now + 60_000}
            }),
            None,
            now,
        )
        .unwrap_err();
        assert_eq!(error.message, "执行提示需为 1–128 KiB");
    }

    #[test]
    fn cron_uses_saved_timezone_and_standard_day_or_weekday_matching() {
        let now = Utc
            .with_ymd_and_hms(2026, 11, 18, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis() as u64;
        let expected_local = Shanghai
            .with_ymd_and_hms(2026, 11, 18, 9, 0, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc)
            .timestamp_millis() as u64;
        assert_eq!(
            next_cron_after("0 9 * * *", Some("Asia/Shanghai"), now),
            Some(expected_local)
        );

        let expected_day_or_weekday = Utc
            .with_ymd_and_hms(2026, 11, 20, 9, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis() as u64;
        assert_eq!(
            next_cron_after("0 9 20 * 1", Some("UTC"), now),
            Some(expected_day_or_weekday)
        );
    }

    #[tokio::test]
    async fn manual_run_claim_is_serialized_per_task() {
        let directory = tempfile::tempdir().unwrap();
        let service = ScheduledTasks::new(directory.path()).unwrap();
        let now = 1_800_000_000_000;
        let task = task_from_payload(
            &json!({
                "name":"手动运行",
                "prompt":"测试",
                "instanceId":"codex:local",
                "model":"gpt-6.1-sol",
                "schedule":{"kind":"once","startAtMs":now + 60_000}
            }),
            None,
            now,
        )
        .unwrap();
        let inserted = service.insert(task).await.unwrap();
        assert_eq!(
            service
                .begin_manual(&inserted.id, now + 1)
                .await
                .unwrap()
                .status,
            "running"
        );
        let error = service
            .begin_manual(&inserted.id, now + 2)
            .await
            .unwrap_err();
        assert_eq!(error.code, 409);
    }
}
