//! 已有会话的一次性定时消息：独立保存，到期后复用普通 send 流程。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{ApiError, HarnessAuthorization, HarnessController};

const MAX_MESSAGES: usize = 256;
const MAX_PROMPT_BYTES: usize = 128 * 1024;
// 定时消息可带图片和文件（正文内联保存），而每次状态变化都整份重写存档：总量必须有上限，
// 与其他持久化存档的单文件上限一致。新增和编辑不能继续扩大超限存档，仍允许缩小与派发状态记录。
const MAX_STORE_BYTES: usize = 64 * 1024 * 1024;
// 用户要求定时发送只允许已有会话，时间按客户端传入的用户本地时间戳保存，最多未来 24 小时。
const MAX_DELAY_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduledMessage {
    id: String,
    thread_id: String,
    payload: Value,
    send_at_ms: u64,
    device_id: String,
    key_id: String,
    status: String,
    #[serde(default)]
    last_error: Option<String>,
    created_at_ms: u64,
    updated_at_ms: u64,
    #[serde(default)]
    dispatch_started: bool,
    #[serde(default)]
    dispatch_id: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    revision: u64,
    messages: Vec<ScheduledMessage>,
}

struct State {
    stored: Stored,
    load_failed: bool,
}

pub(crate) struct ScheduledMessages {
    path: PathBuf,
    state: Mutex<State>,
}

impl ScheduledMessages {
    pub(crate) fn new(directory: &Path) -> anyhow::Result<Self> {
        let path = directory.join("harness-scheduled-messages.json");
        let (stored, load_failed) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
                Ok(mut stored) => {
                    stored.messages.retain(|message| valid_id(&message.id));
                    // 到期派发使用稳定请求 ID；重启后将未完成的调度恢复为可重试状态，
                    // execute 会复用原回执，不会重复发送已受理的普通消息。
                    for message in &mut stored.messages {
                        if message.status == "running" {
                            message.dispatch_started = true;
                            message.status = "scheduled".into();
                        }
                    }
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
            "harness.scheduled_messages_storage_failed",
            json!({"error":format!("{error:#}")}),
        );
    }

    pub(crate) async fn snapshot(&self, thread_id: &str) -> Result<Value, ApiError> {
        let state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时消息存储暂不可用，请重启 Harness 后重试",
            ));
        }
        let mut messages = state
            .stored
            .messages
            .iter()
            .filter(|message| message.thread_id == thread_id)
            .map(public_message)
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| {
            (
                message["sendAtMs"].as_u64().unwrap_or(u64::MAX),
                message["createdAtMs"].as_u64().unwrap_or(u64::MAX),
            )
        });
        Ok(json!({"ok":true,"threadId":thread_id,"messages":messages}))
    }

    pub(crate) async fn action(
        &self,
        controller: &HarnessController,
        action: &str,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        match action {
            "createScheduledMessage" => {
                authorization.check().await?;
                let message = self.create(controller, payload, authorization).await?;
                controller.publish_event(json!({
                    "method":"remote/scheduledMessagesChanged",
                    "params":{"threadId":message.thread_id,"messageId":message.id,"reason":"created"}
                }));
                Ok(json!({"ok":true,"message":public_message(&message)}))
            }
            "cancelScheduledMessage" => {
                authorization.check().await?;
                let thread_id = thread_id(payload)?;
                let message_id = message_id(payload)?;
                self.remove(&thread_id, &message_id).await?;
                controller.publish_event(json!({
                    "method":"remote/scheduledMessagesChanged",
                    "params":{"threadId":thread_id,"messageId":message_id,"reason":"canceled"}
                }));
                Ok(json!({"ok":true,"threadId":thread_id,"messageId":message_id}))
            }
            "updateScheduledMessage" => {
                authorization.check().await?;
                let message = self.update(payload, authorization).await?;
                self.publish_changed(controller, &message, "updated");
                Ok(json!({"ok":true,"message":public_message(&message)}))
            }
            _ => Err(ApiError::new(404, "不支持的定时消息操作").into()),
        }
    }

    pub(crate) async fn tick(self: &Arc<Self>, controller: &Arc<HarnessController>) {
        if controller.closing.load(Ordering::Acquire)
            || !controller.desktop_running.load(Ordering::Acquire)
                && !controller.running.load(Ordering::SeqCst)
        {
            return;
        }
        let due = self.claim_due(super::now_ms()).await;
        for message in &due {
            // 页面据此显示“发送中”并停用取消：认领后消息已进入发送流程。
            self.publish_changed(controller, message, "sending");
        }
        // 用户要求定时发送不拖住后台监视循环：派发会走普通发送流程，含 Claude CLI 冷启动时要数秒到数十秒，
        // 逐条等待会让监视循环里的远程访问核对、定时任务一起延后。按会话分组并发派发，
        // 同一会话内按设置的发送时间先后串行，先到期的先进入普通发送流程。
        for group in group_by_thread(due) {
            let (store, controller) = (self.clone(), controller.clone());
            tokio::spawn(async move {
                for message in group {
                    store.dispatch(&controller, message).await;
                }
            });
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        controller: &Arc<HarnessController>,
        message: ScheduledMessage,
    ) {
        let Some(device) = controller
            .resolve_device(&message.device_id, Some(&message.key_id))
            .await
        else {
            self.finish_error(&message.id, "error", "创建定时消息的设备授权已失效")
                .await;
            self.publish_changed(controller, &message, "failed");
            return;
        };
        let request_id = message
            .dispatch_id
            .clone()
            .unwrap_or_else(|| format!("scheduled-{}", message.id));
        let result = loop {
            let result = controller
                .execute(
                    device.clone(),
                    request_id.clone(),
                    "send".into(),
                    message.payload.clone(),
                )
                .await;
            // execute 最多等 40 秒，超时只表示发送仍在进行（例如 CLI 冷启动较慢）；同一请求 ID 只读回
            // 原回执、不会再次发送，等到有结果再决定删除还是标记。服务关闭时保留“发送中”，重启后按原回执核对。
            if !matches!(&result, Ok(value) if value["state"] == "pending") {
                break result;
            }
            if controller.closing.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        match send_outcome(result) {
            Ok(()) => {
                self.remove_by_id(&message.id).await;
                self.publish_changed(controller, &message, "triggered");
            }
            Err((status, reason)) => {
                self.finish_error(&message.id, status, &reason).await;
                self.publish_changed(controller, &message, "failed");
            }
        }
    }

    pub(crate) async fn remove_thread(&self, thread_id: &str) {
        let mut state = self.state.lock().await;
        if state.load_failed
            || !state
                .stored
                .messages
                .iter()
                .any(|message| message.thread_id == thread_id)
        {
            return;
        }
        if let Err(error) = save_locked(&self.path, &mut state, |stored| {
            stored
                .messages
                .retain(|message| message.thread_id != thread_id);
            Ok(())
        }) {
            Self::log_failure(error);
        }
    }

    async fn create(
        &self,
        controller: &HarnessController,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<ScheduledMessage> {
        let thread_id = thread_id(payload)?;
        let now = super::now_ms();
        let send_at_ms = payload["sendAtMs"]
            .as_u64()
            .ok_or_else(|| ApiError::new(400, "定时发送时间无效"))?;
        if send_at_ms <= now || send_at_ms > now.saturating_add(MAX_DELAY_MS) {
            return Err(ApiError::new(400, "定时发送只能设置在未来 24 小时内").into());
        }
        if payload["goalMode"] == true {
            return Err(ApiError::new(400, "目标模式不支持定时发送").into());
        }
        let prompt = payload["prompt"]
            .as_str()
            .ok_or_else(|| ApiError::new(400, "消息内容无效"))?;
        let has_attachments = payload["images"]
            .as_array()
            .is_some_and(|images| !images.is_empty())
            || payload["files"]
                .as_array()
                .is_some_and(|files| !files.is_empty());
        if prompt.trim().is_empty() && !has_attachments || prompt.len() > MAX_PROMPT_BYTES {
            return Err(ApiError::new(400, "消息不能为空或超过 128 KiB").into());
        }
        super::validate_input_images(payload)?;
        super::input_files::validate_for_device(
            payload,
            authorization.device.key_id == super::desktop::DEVICE_KEY_ID,
        )?;
        // 只核对会话仍然存在：完整详情要读历史（Claude 还要经 WSL 检查占用进程），不能为设置定时消息而加载。
        // 进行中的临时侧边会话没有可单独读取的原生记录，沿用详情接口（也能拒绝已关闭的侧边会话）。
        let detail = controller.harness.known_side(&thread_id).await;
        let response = controller
            .call_backend(
                if detail { "thread" } else { "workspaceContext" },
                json!({"threadId":thread_id}),
                authorization,
            )
            .await?;
        let found = if detail {
            &response["thread"]["id"]
        } else {
            &response["threadId"]
        };
        if found.as_str() != Some(thread_id.as_str()) {
            return Err(ApiError::new(409, "会话身份已变化，请重新打开会话").into());
        }

        let mut message_payload = payload.clone();
        message_payload
            .as_object_mut()
            .ok_or_else(|| ApiError::new(400, "定时消息参数无效"))?
            .remove("sendAtMs");
        message_payload
            .as_object_mut()
            .unwrap()
            .remove("scheduledAtMs");
        let client_id = message_payload["clientUserMessageId"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        message_payload["clientUserMessageId"] = json!(client_id);
        let message = ScheduledMessage {
            id: Uuid::new_v4().to_string(),
            thread_id,
            payload: message_payload,
            send_at_ms,
            device_id: authorization.device.id.clone(),
            key_id: authorization.device.key_id.clone(),
            status: "scheduled".into(),
            last_error: None,
            created_at_ms: now,
            updated_at_ms: now,
            dispatch_started: false,
            dispatch_id: None,
        };
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(503, "定时消息存储暂不可用，请重启 Harness 后重试").into());
        }
        if state.stored.messages.len() >= MAX_MESSAGES {
            return Err(ApiError::new(409, "定时消息数量已达上限").into());
        }
        save_locked(&self.path, &mut state, |stored| {
            stored.messages.push(message.clone());
            Ok(())
        })?;
        Ok(message)
    }

    async fn update(
        &self,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> Result<ScheduledMessage, ApiError> {
        validate_update(payload)?;
        let thread_id = thread_id(payload)?;
        let id = message_id(payload)?;
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时消息存储暂不可用，请重启 Harness 后重试",
            ));
        }
        let message = state
            .stored
            .messages
            .iter()
            .find(|message| message.id == id && message.thread_id == thread_id)
            .ok_or_else(|| ApiError::new(404, "定时消息不存在"))?;
        if let Some(reason) = edit_block(message) {
            return Err(ApiError::new(409, reason));
        }
        // 用户要求修改已创建任务：按打开时的版本原位保存，到期认领与修改共用锁，不能覆盖其他端的更新。
        if payload["expectedUpdatedAtMs"].as_u64() != Some(message.updated_at_ms) {
            return Err(ApiError::new(409, "定时消息已变化，请关闭后重新打开编辑"));
        }
        let now = super::now_ms();
        let send_at = payload["sendAtMs"].as_u64().unwrap();
        if send_at <= now || send_at > now.saturating_add(MAX_DELAY_MS) {
            return Err(ApiError::new(400, "定时发送只能设置在未来 24 小时内"));
        }
        let mut next = message.clone();
        next.payload["prompt"] = payload["prompt"].clone();
        let has_attachments = ["images", "files"].iter().any(|key| {
            next.payload[key]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        });
        if payload["prompt"].as_str().unwrap().trim().is_empty() && !has_attachments {
            return Err(ApiError::new(400, "消息不能为空"));
        }
        // 编辑仅替换正文与时间，原图片、文件、Skill 和模型设置完整保留，并按本次授权核对文件来源。
        super::input_files::validate_for_device(
            &next.payload,
            authorization.device.key_id == super::desktop::DEVICE_KEY_ID,
        )?;
        if message.status == "error" {
            // 仅确定失败才允许重新安排；新派发身份避开旧失败回执，结果未知时禁止借编辑重发。
            next.dispatch_id = Some(format!("scheduled-{}", Uuid::new_v4()));
            next.payload["clientUserMessageId"] = json!(Uuid::new_v4().to_string());
        }
        next.dispatch_started = false;
        next.status = "scheduled".into();
        next.last_error = None;
        next.send_at_ms = send_at;
        next.updated_at_ms = now.max(message.updated_at_ms.saturating_add(1));
        next.device_id = authorization.device.id.clone();
        next.key_id = authorization.device.key_id.clone();
        save_locked(&self.path, &mut state, |stored| {
            *stored
                .messages
                .iter_mut()
                .find(|message| message.id == id)
                .unwrap() = next.clone();
            Ok(())
        })?;
        Ok(next)
    }

    async fn claim_due(&self, now: u64) -> Vec<ScheduledMessage> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Vec::new();
        }
        // 上一批仍在派发的会话暂不认领新到期的消息：派发可能持续数十秒，两批并发会颠倒发送顺序。
        let busy: HashSet<&str> = state
            .stored
            .messages
            .iter()
            .filter(|message| message.status == "running")
            .map(|message| message.thread_id.as_str())
            .collect();
        let mut due: Vec<ScheduledMessage> = state
            .stored
            .messages
            .iter()
            .filter(|message| {
                message.status == "scheduled"
                    && message.send_at_ms <= now
                    && !busy.contains(message.thread_id.as_str())
            })
            .cloned()
            .collect();
        if due.is_empty() {
            return due;
        }
        for message in &mut due {
            message.status = "running".into();
            message.dispatch_started = true;
            message.updated_at_ms = now.max(message.updated_at_ms.saturating_add(1));
        }
        // 状态只在写盘成功后生效：保存失败时仍是“等待发送”，下一轮再认领。
        let claimed: HashSet<&str> = due.iter().map(|message| message.id.as_str()).collect();
        let saved = save_locked(&self.path, &mut state, |stored| {
            for message in &mut stored.messages {
                if claimed.contains(message.id.as_str()) {
                    message.status = "running".into();
                    message.dispatch_started = true;
                    message.updated_at_ms = now.max(message.updated_at_ms.saturating_add(1));
                }
            }
            Ok(())
        });
        if let Err(error) = saved {
            Self::log_failure(error);
            return Vec::new();
        }
        due
    }

    /// `status` 为 "error"（确定没有发出）或 "unknown"（可能已发出，需在会话中核对）。
    async fn finish_error(&self, id: &str, status: &str, reason: &str) {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return;
        }
        if !state.stored.messages.iter().any(|message| message.id == id) {
            return;
        }
        let saved = save_locked(&self.path, &mut state, |stored| {
            if let Some(message) = stored.messages.iter_mut().find(|message| message.id == id) {
                message.status = status.into();
                message.last_error = Some(reason.chars().take(512).collect());
                message.updated_at_ms =
                    super::now_ms().max(message.updated_at_ms.saturating_add(1));
            }
            Ok(())
        });
        if let Err(error) = saved {
            Self::log_failure(error);
        }
    }

    async fn remove(&self, thread_id: &str, id: &str) -> Result<(), ApiError> {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return Err(ApiError::new(
                503,
                "定时消息存储暂不可用，请重启 Harness 后重试",
            ));
        }
        let Some(message) = state
            .stored
            .messages
            .iter()
            .find(|message| message.id == id && message.thread_id == thread_id)
        else {
            return Err(ApiError::new(404, "定时消息不存在"));
        };
        // 已进入发送流程的消息删除记录也拦不住发送，不能让用户误以为取消成功。
        if message.status == "running" || message.status == "scheduled" && message.dispatch_started
        {
            return Err(ApiError::new(409, "定时消息正在发送，无法取消"));
        }
        save_locked(&self.path, &mut state, |stored| {
            stored.messages.retain(|message| message.id != id);
            Ok(())
        })
    }

    async fn remove_by_id(&self, id: &str) {
        let mut state = self.state.lock().await;
        if state.load_failed {
            return;
        }
        let _ = save_locked(&self.path, &mut state, |stored| {
            stored.messages.retain(|message| message.id != id);
            Ok(())
        });
    }

    fn publish_changed(
        &self,
        controller: &HarnessController,
        message: &ScheduledMessage,
        reason: &str,
    ) {
        controller.publish_event(json!({
            "method":"remote/scheduledMessagesChanged",
            "params":{"threadId":message.thread_id,"messageId":message.id,"reason":reason}
        }));
    }
}

/// 到期消息按会话分组：组内按设置的发送时间先后（再按创建时间），组间按各自最早的发送时间。
fn group_by_thread(mut due: Vec<ScheduledMessage>) -> Vec<Vec<ScheduledMessage>> {
    due.sort_by_key(|message| (message.send_at_ms, message.created_at_ms));
    let mut groups: Vec<Vec<ScheduledMessage>> = Vec::new();
    for message in due {
        match groups
            .iter_mut()
            .find(|group| group[0].thread_id == message.thread_id)
        {
            Some(group) => group.push(message),
            None => groups.push(vec![message]),
        }
    }
    groups
}

/// 只有普通发送流程明确完成（含进入队列）才算已触发；失败或结果未知都保留定时消息并写明原因，
/// 不能把“发送结果待确认”当成已发送而悄悄删掉记录，也不能把可能已发出的显示成“发送失败”诱导重发。
fn send_outcome(result: Result<Value, ApiError>) -> Result<(), (&'static str, String)> {
    let unknown = "定时消息发送结果待确认，请在会话中核对后再决定是否重发";
    match result {
        Ok(value) if value["state"] == "completed" => Ok(()),
        Ok(value) => {
            let (status, fallback) = if value["state"] == "failed" {
                ("error", "定时消息发送失败")
            } else {
                ("unknown", unknown)
            };
            let reason = value["result"]["message"].as_str().unwrap_or(fallback);
            Err((status, reason.to_owned()))
        }
        Err(error) if error.outcome_unknown => Err(("unknown", error.to_string())),
        Err(error) => Err(("error", error.to_string())),
    }
}

fn public_message(message: &ScheduledMessage) -> Value {
    let attachment_count: usize = ["images", "files"]
        .iter()
        .map(|key| message.payload[key].as_array().map_or(0, Vec::len))
        .sum();
    json!({
        "id":message.id,
        "threadId":message.thread_id,
        "sendAtMs":message.send_at_ms,
        "prompt":message.payload["prompt"],
        "clientUserMessageId":message.payload["clientUserMessageId"],
        "status":message.status,
        "lastError":message.last_error,
        "createdAtMs":message.created_at_ms,
        "updatedAtMs":message.updated_at_ms,
        "canEdit":edit_block(message).is_none(),
        "editDisabledReason":edit_block(message),
        "canCancel":!(message.status == "running" || message.status == "scheduled" && message.dispatch_started),
        "attachmentCount":attachment_count
    })
}

fn edit_block(message: &ScheduledMessage) -> Option<&'static str> {
    match message.status.as_str() {
        "scheduled" if !message.dispatch_started => None,
        "error" => None,
        "unknown" => Some("发送结果待确认，不能修改或重新安排"),
        _ => Some("定时消息已进入发送流程，无法编辑"),
    }
}

pub(crate) fn validate_update(payload: &Value) -> Result<(), ApiError> {
    if payload.as_object().is_none_or(|object| {
        object.keys().any(|key| {
            ![
                "threadId",
                "messageId",
                "expectedUpdatedAtMs",
                "prompt",
                "sendAtMs",
            ]
            .contains(&key.as_str())
        })
    }) || payload["prompt"]
        .as_str()
        .is_none_or(|text| text.len() > MAX_PROMPT_BYTES)
        || payload["expectedUpdatedAtMs"].as_u64().is_none()
        || payload["sendAtMs"].as_u64().is_none()
    {
        return Err(ApiError::new(400, "定时消息编辑参数无效"));
    }
    thread_id(payload)?;
    message_id(payload)?;
    Ok(())
}

fn thread_id(payload: &Value) -> Result<String, ApiError> {
    super::notification_thread_id(&payload["threadId"])
        .map(str::to_owned)
        .ok_or_else(|| ApiError::new(400, "会话 ID 无效"))
}

fn message_id(payload: &Value) -> Result<String, ApiError> {
    payload["messageId"]
        .as_str()
        .filter(|id| valid_id(id))
        .map(str::to_owned)
        .ok_or_else(|| ApiError::new(400, "定时消息标识无效"))
}

fn valid_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok()
}

fn save_locked(
    path: &Path,
    state: &mut State,
    change: impl FnOnce(&mut Stored) -> anyhow::Result<()>,
) -> Result<(), ApiError> {
    save_locked_within(path, state, MAX_STORE_BYTES, change)
}

fn save_locked_within(
    path: &Path,
    state: &mut State,
    limit: usize,
    change: impl FnOnce(&mut Stored) -> anyhow::Result<()>,
) -> Result<(), ApiError> {
    let mut next = state.stored.clone();
    change(&mut next).map_err(|error| ApiError::new(500, error.to_string()))?;
    next.revision = next
        .revision
        .checked_add(1)
        .ok_or_else(|| ApiError::new(409, "定时消息版本已达上限"))?;
    let bytes = serde_json::to_vec(&next).map_err(|error| ApiError::new(500, error.to_string()))?;
    if bytes.len() > limit
        && next.messages.iter().any(|next| {
            state
                .stored
                .messages
                .iter()
                .find(|old| old.id == next.id)
                .is_none_or(|old| old.payload != next.payload)
        })
        && bytes.len()
            > serde_json::to_vec(&state.stored)
                .map_err(|error| ApiError::new(500, error.to_string()))?
                .len()
    {
        return Err(ApiError::new(
            409,
            "定时消息总大小已达上限，请缩短内容或先删除部分定时消息",
        ));
    }
    crate::settings::atomic_write(path, &bytes)
        .map_err(|error| ApiError::new(503, format!("无法保存定时消息：{error}")))?;
    state.stored = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(
        id: u8,
        thread: &str,
        send_at_ms: u64,
        created_at_ms: u64,
        bytes: usize,
    ) -> ScheduledMessage {
        ScheduledMessage {
            id: Uuid::from_bytes([id; 16]).to_string(),
            thread_id: thread.into(),
            payload: json!({"prompt":"x".repeat(bytes)}),
            send_at_ms,
            device_id: "device".into(),
            key_id: "key".into(),
            status: "scheduled".into(),
            last_error: None,
            created_at_ms,
            updated_at_ms: created_at_ms,
            dispatch_started: false,
            dispatch_id: None,
        }
    }

    #[test]
    fn due_messages_are_grouped_by_session_and_ordered_by_their_scheduled_time() {
        let groups = group_by_thread(vec![
            message(1, "thread-a", 300, 10, 1),
            message(2, "thread-b", 200, 11, 1),
            message(3, "thread-a", 100, 12, 1),
            message(4, "thread-a", 100, 9, 1),
        ]);
        let ids =
            |group: &Vec<ScheduledMessage>| group.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        assert_eq!(groups.len(), 2, "每个会话一组，不同会话可以并发派发");
        assert_eq!(
            groups[0][0].thread_id, "thread-a",
            "组按各自最早的发送时间排列"
        );
        assert_eq!(
            ids(&groups[0]),
            [4, 3, 1].map(|n| Uuid::from_bytes([n; 16]).to_string()),
            "同一会话先按发送时间、同时刻再按创建先后"
        );
        assert_eq!(groups[1].len(), 1);
        assert!(group_by_thread(Vec::new()).is_empty());
    }

    #[test]
    fn only_a_completed_send_counts_as_triggered() {
        assert_eq!(send_outcome(Ok(json!({"state":"completed"}))), Ok(()));
        assert_eq!(
            send_outcome(Ok(
                json!({"state":"failed","result":{"message":"会话已被其他应用打开"}})
            )),
            Err(("error", "会话已被其他应用打开".into()))
        );
        assert_eq!(
            send_outcome(Ok(json!({"state":"failed"}))),
            Err(("error", "定时消息发送失败".into()))
        );
        for state in ["unknown", "pending"] {
            let (status, reason) = send_outcome(Ok(json!({"state":state}))).unwrap_err();
            assert_eq!(status, "unknown", "{state}：可能已发出，不能显示成发送失败");
            assert!(
                reason.contains("待确认"),
                "{state}：结果未知不能当作已发送删除记录"
            );
        }
        assert_eq!(
            send_outcome(Err(ApiError::new(409, "会话繁忙"))),
            Err(("error", "会话繁忙".into()))
        );
        assert_eq!(
            send_outcome(Err(ApiError::new(401, "授权失效").after_admission()))
                .unwrap_err()
                .0,
            "unknown",
            "已受理后授权失效：结果无法确认"
        );
    }

    fn store_with(directory: &Path, messages: Vec<ScheduledMessage>) -> ScheduledMessages {
        let store = ScheduledMessages::new(directory).unwrap();
        let mut state = store.state.try_lock().unwrap();
        save_locked(&store.path, &mut state, |stored| {
            stored.messages = messages;
            Ok(())
        })
        .unwrap();
        drop(state);
        store
    }

    #[tokio::test]
    async fn a_session_waits_for_its_running_batch_and_running_messages_cannot_be_canceled() {
        let directory = tempfile::tempdir().unwrap();
        let store = store_with(
            directory.path(),
            vec![
                message(1, "thread-a", 100, 1, 1),
                message(2, "thread-a", 200, 2, 1),
                message(3, "thread-b", 200, 3, 1),
            ],
        );
        let ids = |due: &[ScheduledMessage]| due.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        let id = |n: u8| Uuid::from_bytes([n; 16]).to_string();
        assert_eq!(ids(&store.claim_due(150).await), vec![id(1)]);
        assert_eq!(
            ids(&store.claim_due(250).await),
            vec![id(3)],
            "同一会话上一条还在发送时不并发认领下一条，其他会话不受影响"
        );
        let refused = store.remove("thread-a", &id(1)).await.unwrap_err();
        assert_eq!(refused.code, 409, "正在发送的消息不能假装取消");
        store.finish_error(&id(1), "unknown", "结果待确认").await;
        assert_eq!(ids(&store.claim_due(250).await), vec![id(2)]);
        store.remove("thread-a", &id(1)).await.unwrap();
        let reloaded = ScheduledMessages::new(directory.path()).unwrap();
        let snapshot = reloaded.snapshot("thread-a").await.unwrap();
        assert_eq!(
            snapshot["messages"][0]["status"], "scheduled",
            "重启后未完成的发送恢复为等待发送，由原回执去重"
        );
        assert_eq!(
            snapshot["messages"][0]["canEdit"], false,
            "原回执尚未核对时不能通过编辑改写在途发送"
        );
        assert_eq!(snapshot["messages"][0]["canCancel"], false);
    }

    #[tokio::test]
    async fn edits_keep_payload_and_identity_check_versions_and_serialize_with_dispatch() {
        let (directory, authorization) = crate::tests::service_dispatch_fixture().await;
        let thread = "01234567-89ab-4cde-8123-456789abcdef";
        let mut original = message(1, thread, super::super::now_ms() + 60_000, 1, 1);
        original.payload = json!({"threadId":thread,"prompt":"原内容","clientUserMessageId":"original",
            "images":[{"dataUrl":"data:image/png;base64,AQ=="}],
            "skills":[{"name":"test","path":"C:/test/SKILL.md"}],"model":"test-model","reasoningEffort":"high"});
        let store = store_with(directory.path(), vec![original.clone()]);
        let request = json!({"threadId":thread,"messageId":original.id,"expectedUpdatedAtMs":1,
            "prompt":"修改内容","sendAtMs":super::super::now_ms() + 120_000});
        let changed = store.update(&request, &authorization).await.unwrap();
        assert_eq!(changed.id, original.id);
        assert_eq!(changed.payload["prompt"], "修改内容");
        for key in [
            "images",
            "skills",
            "model",
            "reasoningEffort",
            "clientUserMessageId",
        ] {
            assert_eq!(
                changed.payload[key], original.payload[key],
                "{key} 必须保留"
            );
        }
        assert!(changed.updated_at_ms > original.updated_at_ms);
        assert_eq!(
            store
                .update(&request, &authorization)
                .await
                .unwrap_err()
                .code,
            409,
            "旧页面不能覆盖新修改"
        );
        let mut invalid = request.clone();
        invalid["expectedUpdatedAtMs"] = json!(changed.updated_at_ms);
        invalid["sendAtMs"] = json!(super::super::now_ms() + MAX_DELAY_MS + 60_000);
        assert_eq!(
            store
                .update(&invalid, &authorization)
                .await
                .unwrap_err()
                .code,
            400
        );
        invalid["sendAtMs"] = json!(1);
        assert_eq!(
            store
                .update(&invalid, &authorization)
                .await
                .unwrap_err()
                .code,
            400
        );
        invalid["sendAtMs"] = changed.send_at_ms.into();
        invalid["threadId"] = json!("01234567-89ab-4cde-8123-456789abcdee");
        assert_eq!(
            store
                .update(&invalid, &authorization)
                .await
                .unwrap_err()
                .code,
            404
        );
        assert!(
            validate_update(&json!({"threadId":thread,"messageId":original.id,
            "expectedUpdatedAtMs":1,"prompt":"x","sendAtMs":1,"model":"replace"}))
            .is_err()
        );
        let reloaded = ScheduledMessages::new(directory.path()).unwrap();
        assert_eq!(
            reloaded.snapshot(thread).await.unwrap()["messages"][0]["prompt"],
            "修改内容"
        );
        assert!(
            store.claim_due(changed.send_at_ms - 1).await.is_empty(),
            "改期后旧时间不派发"
        );
        assert_eq!(store.claim_due(changed.send_at_ms).await.len(), 1);
        assert_eq!(
            store
                .update(&request, &authorization)
                .await
                .unwrap_err()
                .code,
            409
        );
        store
            .finish_error(&changed.id, "unknown", "可能已发出")
            .await;
        assert_eq!(
            store
                .update(&request, &authorization)
                .await
                .unwrap_err()
                .code,
            409
        );
    }

    struct SendBridge {
        calls: std::sync::Mutex<Vec<(String, Value)>>,
        send_fails: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl crate::TestRemoteBackend for SendBridge {
        async fn call(&self, action: &str, payload: Value) -> anyhow::Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((action.to_string(), payload.clone()));
            if action == "send" && self.send_fails.load(Ordering::SeqCst) {
                return Err(ApiError::new(409, "会话已被其他应用打开").into());
            }
            Ok(json!({"ok":true,"threadId":payload["threadId"]}))
        }
    }

    #[tokio::test]
    async fn a_scheduled_message_is_sent_as_an_ordinary_message_when_due() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = Arc::new(SendBridge {
            calls: Default::default(),
            send_fails: Default::default(),
        });
        // 访问密钥须在控制器创建前写入，控制器只在启动时读取。
        crate::credentials::configure_access_key(directory.path(), "scheduled-test-key").unwrap();
        let controller =
            HarnessController::with_test_backend(bridge.clone(), directory.path().to_path_buf())
                .unwrap();
        controller.publish_access_policy(&crate::RemoteAccessSettings {
            enabled: true,
            ..Default::default()
        });
        controller.running.store(true, Ordering::SeqCst);
        let device = {
            let mut auth = controller.auth.lock().await;
            let device = crate::Device {
                id: "scheduled-device".into(),
                name: "定时发送测试".into(),
                token_hash: crate::hash("scheduled-cookie"),
                client_id_hash: String::new(),
                created_at_ms: crate::now_ms(),
                expires_at_ms: crate::now_ms() + 3_600_000,
                last_seen_at_ms: crate::now_ms(),
                key_id: auth.access_key.as_ref().unwrap().id.clone(),
                public_origin: None,
            };
            auth.devices.push(device.clone());
            device
        };
        let thread_id = "01234567-89ab-4cde-8123-456789abcdef";
        let store = controller.scheduled_messages.clone();
        let schedule = |request: &str, prompt: &str| {
            controller.execute(
                device.clone(),
                request.into(),
                "createScheduledMessage".into(),
                json!({"threadId":thread_id,"prompt":prompt,"clientUserMessageId":request,
                    "sendAtMs":crate::now_ms() + 60_000,"scheduledAtMs":crate::now_ms() + 60_000}),
            )
        };
        // 首条走真实的 HTTP 操作入口，载荷字段与网页一致，覆盖字段白名单和输入校验。
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("codex_elves_device=scheduled-cookie"),
        );
        headers.insert(
            "idempotency-key",
            axum::http::HeaderValue::from_static("create-1"),
        );
        let send_at = crate::now_ms() + 60_000;
        let created = crate::action(
            axum::extract::State(crate::WebState {
                desktop: false,
                controller: controller.clone(),
                settings: Default::default(),
            }),
            axum::extract::Path("createScheduledMessage".to_string()),
            headers,
            axum::Json(json!({"prompt":"到点发送","instanceId":"codex:local","threadId":thread_id,
                "clientUserMessageId":"create-1","sendAtMs":send_at,"scheduledAtMs":send_at,
                "model":"gpt-fixture","reasoningEffort":"high","skills":[{"name":"s","path":"C:/s/SKILL.md"}]})),
        )
        .await
        .map_err(|error| error.to_string())
        .unwrap()
        .0;
        assert_eq!(created["state"], "completed", "{created}");
        let original = &created["result"]["message"];
        let mut edit_headers = axum::http::HeaderMap::new();
        edit_headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("codex_elves_device=scheduled-cookie"),
        );
        edit_headers.insert(
            "idempotency-key",
            axum::http::HeaderValue::from_static("edit-1"),
        );
        let edited = crate::action(
            axum::extract::State(crate::WebState {
                desktop: false,
                controller: controller.clone(),
                settings: Default::default(),
            }),
            axum::extract::Path("updateScheduledMessage".to_string()),
            edit_headers,
            axum::Json(json!({"threadId":thread_id,"messageId":original["id"],
                "expectedUpdatedAtMs":original["updatedAtMs"],"prompt":"修改后到点发送",
                "sendAtMs":crate::now_ms() + 90_000})),
        )
        .await
        .map_err(|error| error.to_string())
        .unwrap()
        .0;
        assert_eq!(edited["state"], "completed", "{edited}");
        assert_eq!(
            edited["result"]["message"]["id"], original["id"],
            "编辑原位保存，不删除再创建"
        );
        assert!(
            store.claim_due(crate::now_ms()).await.is_empty(),
            "未到时间不发送"
        );
        let due = store.claim_due(crate::now_ms() + 120_000).await;
        assert_eq!(due.len(), 1);
        store.dispatch(&controller, due[0].clone()).await;
        let sends = || {
            bridge
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(action, _)| action == "send")
                .map(|(_, payload)| payload.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(sends().len(), 1);
        assert_eq!(sends()[0]["prompt"], "修改后到点发送");
        assert_eq!(sends()[0]["clientUserMessageId"], "create-1");
        assert_eq!(sends()[0]["skills"][0]["name"], "s");
        assert_eq!(sends()[0]["model"], "gpt-fixture");
        assert!(sends()[0].get("sendAtMs").is_none() && sends()[0].get("scheduledAtMs").is_none());
        let empty = store.snapshot(thread_id).await.unwrap();
        assert_eq!(empty["messages"], json!([]), "发送完成后删除定时消息");

        bridge.send_fails.store(true, Ordering::SeqCst);
        assert_eq!(
            schedule("create-2", "会失败").await.unwrap()["state"],
            "completed"
        );
        let due = store.claim_due(crate::now_ms() + 120_000).await;
        store.dispatch(&controller, due[0].clone()).await;
        let failed = store.snapshot(thread_id).await.unwrap();
        assert_eq!(failed["messages"][0]["status"], "error", "{failed}");
        assert_eq!(failed["messages"][0]["lastError"], "会话已被其他应用打开");
        assert_eq!(sends().len(), 2, "失败后不自动重发");
        assert!(
            store.claim_due(crate::now_ms() + 120_000).await.is_empty(),
            "失败的定时消息不再自动认领"
        );
        let failed = &failed["messages"][0];
        let rescheduled = controller.execute(device, "edit-failed".into(), "updateScheduledMessage".into(),
            json!({"threadId":thread_id,"messageId":failed["id"],"expectedUpdatedAtMs":failed["updatedAtMs"],
                "prompt":"修改后重试","sendAtMs":crate::now_ms() + 60_000})).await.unwrap();
        assert_eq!(rescheduled["state"], "completed", "{rescheduled}");
        bridge.send_fails.store(false, Ordering::SeqCst);
        let due = store.claim_due(crate::now_ms() + 120_000).await;
        store.dispatch(&controller, due[0].clone()).await;
        assert_eq!(
            sends().len(),
            3,
            "明确失败后编辑重新安排使用新回执，实际只多发送一次"
        );
        assert_eq!(sends()[2]["prompt"], "修改后重试");
        assert_ne!(sends()[2]["clientUserMessageId"], "create-2");
        assert!(
            store.snapshot(thread_id).await.unwrap()["messages"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_store_cannot_grow_past_its_limit_but_can_always_shrink() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("harness-scheduled-messages.json");
        let mut state = State {
            stored: Stored::default(),
            load_failed: false,
        };
        let limit = 3000;
        save_locked_within(&path, &mut state, limit, |stored| {
            stored.messages.push(message(1, "thread-a", 100, 1, 1200));
            Ok(())
        })
        .unwrap();
        save_locked_within(&path, &mut state, limit, |stored| {
            stored.messages.push(message(2, "thread-a", 200, 2, 1200));
            Ok(())
        })
        .unwrap();
        let refused = save_locked_within(&path, &mut state, limit, |stored| {
            stored.messages.push(message(3, "thread-a", 300, 3, 1200));
            Ok(())
        })
        .unwrap_err();
        assert_eq!(refused.code, 409, "总量超限时拒绝新增");
        assert_eq!(state.stored.messages.len(), 2, "被拒绝的新增不留痕");
        assert!(
            std::fs::read(&path).unwrap().len() <= limit,
            "磁盘上的存档也没有被写成超限内容"
        );
        let refused_edit = save_locked_within(&path, &mut state, limit, |stored| {
            stored.messages[0].payload["prompt"] = json!("x".repeat(5000));
            Ok(())
        })
        .unwrap_err();
        assert_eq!(refused_edit.code, 409, "编辑不能通过保持条数绕过存储上限");
        assert_eq!(
            state.stored.messages[0].payload["prompt"]
                .as_str()
                .unwrap()
                .len(),
            1200
        );
        // 存档已经超限（例如以前没有上限时写入）：取消、到期清理等缩小操作仍必须成功。
        save_locked_within(&path, &mut state, 100, |stored| {
            stored.messages.remove(0);
            Ok(())
        })
        .unwrap();
        assert_eq!(state.stored.messages.len(), 1);
    }
}
