use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::{HarnessAuthorization, now_ms};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Queued {
    pub id: String,
    pub payload: Value,
    // 手动压缩与普通消息共用顺序，但不能作为用户正文发送或取回编辑。
    #[serde(default)]
    pub compact: bool,
    pub created_at: u64,
    pub device_id: String,
    pub key_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl Queued {
    pub fn new(mut payload: Value, authorization: &HarnessAuthorization) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        if payload["clientUserMessageId"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            payload["clientUserMessageId"] = json!(uuid::Uuid::new_v4().to_string());
        }
        Self {
            id,
            payload,
            compact: false,
            created_at: now_ms(),
            device_id: authorization.device.id.clone(),
            key_id: authorization.device.key_id.clone(),
            state: "queued".into(),
            request_id: authorization.request_id.clone(),
        }
    }

    pub fn view(&self) -> Value {
        let images: Vec<_> = self.payload["images"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|image| json!({"src":image.get("localPath").unwrap_or(&image["dataUrl"]),"name":image["name"]}))
            .collect();
        let files: Vec<_> = self.payload["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| {
                Some(json!({
                    "kind": file["kind"].as_str()?,
                    "name": file["name"].as_str()?,
                    "size": file["size"],
                    "path": file["path"],
                    "staged": file.get("staged"),
                }))
            })
            .collect();
        json!({"id":self.id,"clientUserMessageId":self.payload["clientUserMessageId"],
            "action":if self.compact {"compact"} else {"send"},
            "text":if self.compact {json!("压缩上下文")} else {self.payload["prompt"].clone()},
            "prompt":self.payload["prompt"],"createdAt":self.created_at,
            "canSendNow":matches!(self.state.as_str(),"queued"|"paused"),"pausedReason":match self.state.as_str() {
                "unknown" => json!("消息送达状态待核对"), "paused" => json!("消息已暂停"), _ => Value::Null },
            "remoteDeliveryState":if self.state=="unknown" {"outcome-unknown"} else {"queued"},
            "submission":{"status":match self.state.as_str(){"unknown"=>"outcome-unknown","sending"=>"sending",_=>"queued"}},
            "context":{"prompt":self.payload["prompt"],"imageAttachments":images,"fileAttachments":files},
            "submissionOptions":{"clientUserMessageId":self.payload["clientUserMessageId"],
                "model":self.payload["model"],"reasoningEffort":self.payload["reasoningEffort"]}})
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Edit {
    pub thread_id: String,
    pub message_id: String,
    pub state: String,
    pub original: Queued,
    pub result: Value,
    #[serde(default)]
    pub terminal_fingerprint: Option<String>,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct QueueGate {
    pub turn_id: Option<String>,
    pub kind: String,
    pub phase: String,
    pub reason: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Deletion {
    pub state: String,
    pub device_id: String,
    pub request_id: Option<String>,
    #[serde(default)]
    pub cleanup_pending: bool,
    // 原生客户端创建的持久侧边也要保留父关联；结果未知后退出列表仍须保护父会话删除。
    #[serde(default)]
    pub attached_to_thread_id: Option<String>,
}

/// 用户要求（2026-11-21）：Codex 的后台终端（exec_command 让出后仍在运行的命令）也算执行中，退出或升级时
/// 不能被静默结束。协议没有列出后台终端的接口，thread/read 在命令结束前也看不到它，只能按 item/started 与
/// item/completed 跟踪；Harness 交接时随私有服务代次保存在这里，新 Harness 只在接上同一代次服务时沿用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OpenCommand {
    pub thread_id: String,
    pub item_id: String,
    pub command: String,
    #[serde(default)]
    pub process_id: Option<String>,
    /// 私有服务代次；stdio 连接为空，Harness 一断开就随 app-server 结束。
    #[serde(default)]
    pub server: Option<String>,
    /// 由上一个 Harness 交接而来：本连接收不到它的 item/completed，需要用 thread/read 核对是否已结束。
    #[serde(default)]
    pub inherited: bool,
}

/// 手动压缩临时切换压缩模型时的恢复记录；压缩完成、失败、中断或服务重启后必须切回原会话模型。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CompactionModelRestore {
    pub original_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_service_tier: Option<String>,
    pub compaction_model: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct Data {
    pub queues: BTreeMap<String, Vec<Queued>>,
    pub edits: BTreeMap<String, Edit>,
    pub sent: BTreeMap<String, Value>,
    pub sent_hashes: BTreeMap<String, String>,
    pub thread_pins: BTreeSet<String>,
    pub project_pins: BTreeSet<String>,
    pub project_pin_overrides: BTreeMap<String, bool>,
    pub project_pin_desktop_revisions: BTreeMap<String, String>,
    // 用户从 Elves“添加文件夹”登记的项目由 Harness 自己保存，不写入 Codex App 偏好。
    pub local_projects: BTreeMap<String, Value>,
    pub preferences: Value,
    pub settings: BTreeMap<String, Value>,
    // 持久分支保留可点击的来源和家族标题；临时侧边不写入此表。
    pub fork_sources: BTreeMap<String, Value>,
    // 持久侧边只保存创建时权限与协作模式；正文、父子关系与历史由原生 attached thread 持有。
    pub persistent_sides: BTreeMap<String, super::persistent_sides::PersistentSide>,
    pub answered_questions: BTreeMap<String, Value>,
    pub thread_devices: BTreeMap<String, String>,
    pub queue_gates: BTreeMap<String, QueueGate>,
    pub deletions: BTreeMap<String, Deletion>,
    /// 交接给下一个 Harness 的后台终端，键为 `会话ID:条目ID`；见 OpenCommand。
    pub background_commands: BTreeMap<String, OpenCommand>,
    /// 手动压缩临时切往独立压缩模型后待恢复的原会话模型，按会话持久化防止重启遗留。
    pub compaction_model_restores: BTreeMap<String, CompactionModelRestore>,
    /// 按原生会话、轮次、条目保存压缩证据；历史有对应条目才附加，不据此复活轮次。
    pub compaction_details: BTreeMap<String, Vec<Value>>,
}

pub(super) struct Store {
    pub path: PathBuf,
    pub data: Data,
    pub revision: u64,
}

impl Store {
    pub fn open(directory: &Path) -> anyhow::Result<Self> {
        let path = directory.join("remote-harness-state.json");
        let mut data: Data = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("远程会话服务状态文件损坏，未覆盖原文件")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(error) => return Err(error.into()),
        };
        // 用户要求重连后可继续操作；已派发但无回执的消息必须待核对，不能重发。
        // 删除保护按会话持久保存，不能因刷新、换设备或服务重启丢失原操作的未知状态。
        for deletion in data.deletions.values_mut() {
            if deletion.state == "pending" {
                deletion.state = "unknown".into();
            }
        }
        for queue in data.queues.values_mut() {
            for message in queue
                .iter_mut()
                .filter(|message| matches!(message.state.as_str(), "sending" | "direct-sending"))
            {
                message.state = if message.state == "direct-sending" {
                    "direct-unknown"
                } else {
                    "unknown"
                }
                .into();
            }
        }
        // 用户要求异常退出后不自动续发：保留上一轮依赖，重启后重新核对正常完成凭据。
        // 旧版本没有记录依赖轮次的队列也需要核对，不能把恢复后的 idle 当成正常结束。
        for (id, queue) in &data.queues {
            if queue.iter().any(|message| message.state == "queued") {
                let gate = data
                    .queue_gates
                    .entry(id.clone())
                    .or_insert_with(|| QueueGate {
                        kind: "turn".into(),
                        phase: "recovering".into(),
                        ..Default::default()
                    });
                if gate.phase == "waiting" {
                    gate.phase = "recovering".into();
                }
            }
        }
        Ok(Self {
            path,
            data,
            revision: 1,
        })
    }

    pub fn commit<T>(
        &mut self,
        change: impl FnOnce(&mut Data) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut next = self.data.clone();
        let value = change(&mut next)?;
        next.queue_gates.retain(|id, _| {
            next.queues.get(id).is_some_and(|queue| {
                queue
                    .iter()
                    .any(|message| !message.state.starts_with("direct-"))
            })
        });
        while next.sent.len() > 4096 {
            next.sent.pop_first();
        }
        next.sent_hashes.retain(|id, _| next.sent.contains_key(id));
        // 已结束编辑只保留回执和内容指纹，不能长期重复保存原图片挤占待发容量。
        let completed: Vec<_> = next
            .edits
            .iter_mut()
            .filter(|(_, edit)| matches!(edit.state.as_str(), "committed" | "canceled"))
            .map(|(id, edit)| {
                edit.original.payload = json!({"prompt":""});
                id.clone()
            })
            .collect();
        for id in completed.iter().take(completed.len().saturating_sub(256)) {
            next.edits.remove(id);
        }
        while next.answered_questions.len() > 4096 {
            next.answered_questions.pop_first();
        }
        let bytes = serde_json::to_vec(&next)?;
        anyhow::ensure!(
            bytes.len() <= 64 * 1024 * 1024,
            "远程待发送内容已达 64 MiB，请先处理现有队列"
        );
        crate::settings::atomic_write(&self.path, &bytes)?;
        self.data = next;
        self.revision += 1;
        Ok(value)
    }

    pub fn queue(&self, thread_id: &str, epoch: &str) -> Value {
        self.data.queue(thread_id, epoch, self.revision)
    }
}

impl Data {
    /// 用户确认历史 inProgress、已完成编辑回执和编辑草稿不代表真实执行。
    /// 退出只应被待派发、发送中、结果未知或其他仍需处理的队列状态阻塞；
    /// 草稿、完成/取消回执及仅绑定草稿的 queue gate 必须保留数据但允许 Harness 退出。
    /// 用户要求安装/退出被拦截时直接给出可排查的原因：逐条列出仍需派发、发送中、
    /// 结果未知的队列消息，以及未完成的编辑和删除，不能只返回一个布尔值。
    /// queue gate 只有在同一会话仍有非草稿消息时才会阻塞，此时已由 queue_pending 覆盖，
    /// 所以只作为附加信息输出。
    pub fn release_blockers(&self) -> Vec<Value> {
        let mut blockers = Vec::new();
        for (thread_id, messages) in &self.queues {
            let states: Vec<&str> = messages
                .iter()
                .filter(|message| message.state != "editing")
                .map(|message| message.state.as_str())
                .collect();
            if !states.is_empty() {
                blockers.push(json!({
                    "kind":"queue_pending",
                    "threadId":thread_id,
                    "states":states,
                    "gate":self.queue_gates.get(thread_id).map(|gate| gate.phase.as_str())
                }));
            }
        }
        for (edit_id, edit) in &self.edits {
            if !matches!(edit.state.as_str(), "editing" | "committed" | "canceled") {
                blockers.push(json!({
                    "kind":"edit_pending",
                    "threadId":edit.thread_id,
                    "editId":edit_id,
                    "state":edit.state
                }));
            }
        }
        for (thread_id, deletion) in &self.deletions {
            if deletion.state != "deleted" {
                blockers.push(json!({
                    "kind":"deletion_pending",
                    "threadId":thread_id,
                    "state":deletion.state
                }));
            }
        }
        blockers
    }

    pub fn queue(&self, thread_id: &str, epoch: &str, revision: u64) -> Value {
        let messages: Vec<_> = self
            .queues
            .get(thread_id)
            .into_iter()
            .flatten()
            .filter(|q| q.state != "editing" && !q.state.starts_with("direct-"))
            .map(|message| {
                let mut view = message.view();
                if let Some(gate) = self.queue_gates.get(thread_id)
                    && (message.state == "paused"
                        || message.state == "queued" && gate.phase == "recovering")
                    && !gate.reason.is_empty()
                {
                    view["pausedReason"] = json!(&gate.reason);
                }
                view
            })
            .collect();
        let edits: Vec<_> = self.edits.iter().filter(|(_, e)| e.thread_id == thread_id
            && !["committed","canceled"].contains(&e.state.as_str()))
            .map(|(id,e)| json!({"editId":id,"messageId":e.message_id,"state":e.state,
                "editState":e.state,"originalText":e.original.payload["prompt"],"createdAt":e.original.created_at})).collect();
        json!({"ok":true,"threadId":thread_id,"queueEpoch":epoch,"queueRevision":revision,
            "queuedMessages":messages,"queuedEdits":edits})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_ignores_history_and_editing_drafts_but_keeps_pending_work_blocked() {
        let queued = |id: &str, prompt: &str| Queued {
            id: id.into(),
            payload: json!({"prompt":prompt,"clientUserMessageId":id}),
            compact: false,
            created_at: 1,
            device_id: "device".into(),
            key_id: "key".into(),
            state: "queued".into(),
            request_id: None,
        };
        let mut data = Data::default();
        data.edits.insert(
            "completed".into(),
            Edit {
                thread_id: "draft-thread".into(),
                message_id: "completed-message".into(),
                state: "committed".into(),
                original: queued("completed-message", "done"),
                result: json!({"ok":true}),
                terminal_fingerprint: Some("fingerprint".into()),
            },
        );
        data.edits.insert(
            "editing".into(),
            Edit {
                thread_id: "draft-thread".into(),
                message_id: "editing-message".into(),
                state: "editing".into(),
                original: queued("editing-message", "draft"),
                result: Value::Null,
                terminal_fingerprint: None,
            },
        );
        data.queues.insert(
            "draft-thread".into(),
            vec![Queued {
                state: "editing".into(),
                ..queued("draft-message", "draft")
            }],
        );
        data.queue_gates.insert(
            "draft-thread".into(),
            QueueGate {
                phase: "paused".into(),
                ..Default::default()
            },
        );
        assert!(
            data.release_blockers().is_empty(),
            "历史回执、编辑草稿和仅绑定草稿的 gate 不能阻塞退出"
        );

        data.queues.get_mut("draft-thread").unwrap()[0].state = "queued".into();
        assert!(
            !data.release_blockers().is_empty(),
            "待发送队列必须继续阻塞退出"
        );
        data.deletions.insert(
            "deleting-thread".into(),
            Deletion {
                state: "unknown".into(),
                device_id: "device".into(),
                request_id: None,
                cleanup_pending: false,
                attached_to_thread_id: None,
            },
        );
        // 安装器和托盘退出要逐条展示原因，明细必须带出会话 ID 与具体状态。
        assert_eq!(
            data.release_blockers(),
            vec![
                json!({"kind":"queue_pending","threadId":"draft-thread",
                    "states":["queued"],"gate":"paused"}),
                json!({"kind":"deletion_pending","threadId":"deleting-thread","state":"unknown"}),
            ]
        );
    }

    #[test]
    fn harness_store_restart_never_replays_an_unconfirmed_dispatch() {
        for state in ["sending", "direct-sending"] {
            let directory = tempfile::tempdir().unwrap();
            let mut store = Store::open(directory.path()).unwrap();
            store
                .commit(|data| {
                    data.queues.insert(
                        "thread-a".into(),
                        vec![Queued {
                            compact: false,
                            id: "message-a".into(),
                            payload: json!({"prompt":"test","clientUserMessageId":"client-a"}),
                            created_at: 1,
                            device_id: "device-a".into(),
                            key_id: "key-a".into(),
                            state: state.into(),
                            request_id: None,
                        }],
                    );
                    Ok(())
                })
                .unwrap();
            let reopened = Store::open(directory.path()).unwrap();
            assert_eq!(
                reopened.data.queues["thread-a"][0].state,
                if state == "sending" {
                    "unknown"
                } else {
                    "direct-unknown"
                }
            );
            let view = reopened.queue("thread-a", "epoch");
            if state == "direct-sending" {
                assert!(
                    view["queuedMessages"].as_array().unwrap().is_empty(),
                    "直接发送意图重启后继续只读核对，不能变成待发送队列"
                );
                continue;
            }
            assert_eq!(view["queuedMessages"][0]["canSendNow"], false);
            assert_eq!(
                view["queuedMessages"][0]["submission"]["status"],
                "outcome-unknown"
            );
        }
    }

    #[test]
    fn harness_store_restart_preserves_delete_identity_and_unknown_protection() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        store
            .commit(|data| {
                for state in ["pending", "unknown", "deleted"] {
                    data.deletions.insert(
                        state.into(),
                        Deletion {
                            state: state.into(),
                            device_id: "device".into(),
                            request_id: Some(format!("delete-{state}")),
                            cleanup_pending: false,
                            attached_to_thread_id: Some("parent".into()),
                        },
                    );
                }
                Ok(())
            })
            .unwrap();
        let reopened = Store::open(directory.path()).unwrap();
        assert_eq!(reopened.data.deletions["pending"].state, "unknown");
        assert_eq!(reopened.data.deletions["pending"].attached_to_thread_id.as_deref(), Some("parent"));
        assert_eq!(
            reopened.data.deletions["pending"].request_id.as_deref(),
            Some("delete-pending")
        );
        assert_eq!(reopened.data.deletions["unknown"].state, "unknown");
        assert_eq!(reopened.data.deletions["deleted"].state, "deleted");
        assert_eq!(reopened.data.deletions.len(), 3);
    }

    #[test]
    fn harness_store_failed_persistence_does_not_publish_a_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path()).unwrap();
        std::fs::write(directory.path().join("blocked"), "file").unwrap();
        store.path = directory.path().join("blocked").join("state.json");
        let revision = store.revision;
        assert!(
            store
                .commit(|data| {
                    data.thread_pins.insert("thread-a".into());
                    Ok(())
                })
                .is_err()
        );
        assert!(store.data.thread_pins.is_empty());
        assert_eq!(store.revision, revision);
    }
}
