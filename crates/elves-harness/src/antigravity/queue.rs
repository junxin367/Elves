//! Antigravity 会话的远程待发送队列：由 Harness 侧持久化并在轮次空闲时按序通过 ACP `session/prompt` 投递。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::ApiError;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct AntigravityQueue {
    pub messages: Vec<QueuedMessage>,
    pub edits: BTreeMap<String, QueuedEdit>,
    pub revision: u64,
    pub paused: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueuedMessage {
    pub id: String,
    pub payload: Value,
    pub created_at: u64,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueuedEdit {
    pub original: QueuedMessage,
    pub state: String,
    pub result: Value,
}

impl QueuedMessage {
    pub(crate) fn view(&self, paused: bool) -> Value {
        let images: Vec<_> = self.payload["images"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|i| json!({ "src": i["dataUrl"], "name": i["name"] }))
            .collect();
        let status = match self.state.as_str() {
            "sending" => "sending",
            "unknown" => "outcome-unknown",
            _ => "queued",
        };
        json!({
            "id": self.id,
            "clientUserMessageId": self.payload["clientUserMessageId"],
            "action": "send",
            "text": self.payload["prompt"],
            "createdAt": self.created_at,
            "remoteQueueSource": "remote",
            "canSendNow": matches!(self.state.as_str(), "queued" | "paused" | ""),
            "pausedReason": if !self.reason.is_empty() {
                json!(self.reason)
            } else if paused {
                json!("队列已暂停")
            } else {
                Value::Null
            },
            "remoteDeliveryState": if status == "outcome-unknown" { "outcome-unknown" } else { "queued" },
            "submission": { "status": status },
            "context": {
                "prompt": self.payload["prompt"],
                "imageAttachments": images,
            },
            "submissionOptions": {
                "clientUserMessageId": self.payload["clientUserMessageId"],
                "model": self.payload["model"],
                "reasoningEffort": self.payload["reasoningEffort"],
            }
        })
    }
}

impl AntigravityQueue {
    pub(crate) fn recover(&mut self) {
        let mut changed = false;
        for msg in &mut self.messages {
            if msg.state == "sending" {
                msg.state = "paused".into();
                msg.reason = "服务曾重启，队列已暂停，请确认后继续发送".into();
                self.paused = true;
                changed = true;
            }
        }
        if changed {
            self.revision += 1;
        }
    }

    pub(crate) fn pause(&mut self, reason: &str) {
        self.paused = true;
        if !reason.is_empty() {
            for msg in &mut self.messages {
                if msg.state.is_empty() || msg.state == "queued" {
                    msg.state = "paused".into();
                    msg.reason = reason.to_owned();
                }
            }
        }
        self.revision += 1;
    }

    pub(crate) fn enqueue(&mut self, payload: Value, now_ms: u64) -> QueuedMessage {
        let id = payload["clientUserMessageId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Some(existing) = self.messages.iter().find(|m| m.id == id) {
            return existing.clone();
        }
        let msg = QueuedMessage {
            id,
            payload,
            created_at: now_ms,
            state: "queued".into(),
            reason: String::new(),
        };
        self.messages.push(msg.clone());
        self.revision += 1;
        msg
    }

    pub(crate) fn pop_next_ready(&mut self) -> Option<QueuedMessage> {
        if self.paused || !self.edits.values().all(|e| e.state != "editing") {
            return None;
        }
        let idx = self
            .messages
            .iter()
            .position(|m| matches!(m.state.as_str(), "" | "queued"))?;
        let msg = self.messages.remove(idx);
        self.revision += 1;
        Some(msg)
    }

    pub(crate) fn take_by_id(&mut self, id: &str) -> Result<QueuedMessage, ApiError> {
        let idx = self
            .messages
            .iter()
            .position(|m| m.id == id)
            .ok_or_else(|| ApiError::new(404, "待发送消息已不在队列中"))?;
        let msg = self.messages.remove(idx);
        self.revision += 1;
        Ok(msg)
    }

    pub(crate) fn remove(&mut self, id: &str) -> Result<(), ApiError> {
        let before = self.messages.len();
        self.messages.retain(|m| m.id != id);
        if self.messages.len() == before {
            return Err(ApiError::new(404, "待发送消息已不在队列中"));
        }
        self.revision += 1;
        Ok(())
    }

    pub(crate) fn reorder(&mut self, ordered_ids: &[String]) -> Result<(), ApiError> {
        if ordered_ids.len() != self.messages.len() {
            return Err(ApiError::new(409, "队列内容已变化，请刷新后重试排序"));
        }
        let mut by_id: BTreeMap<String, QueuedMessage> = self
            .messages
            .drain(..)
            .map(|m| (m.id.clone(), m))
            .collect();
        let mut next = Vec::with_capacity(ordered_ids.len());
        for id in ordered_ids {
            let Some(msg) = by_id.remove(id) else {
                // 还原
                self.messages = next.into_iter().chain(by_id.into_values()).collect();
                return Err(ApiError::new(409, "队列排序标识无效或已变化"));
            };
            next.push(msg);
        }
        self.messages = next;
        self.revision += 1;
        Ok(())
    }

    pub(crate) fn begin_edit(
        &mut self,
        message_id: &str,
        edit_id: &str,
    ) -> Result<Value, ApiError> {
        if let Some(existing) = self.edits.get(edit_id) {
            return Ok(json!({
                "ok": true,
                "editId": edit_id,
                "state": existing.state,
                "message": existing.original.view(self.paused),
            }));
        }
        let msg = self.take_by_id(message_id)?;
        let view = msg.view(self.paused);
        self.edits.insert(
            edit_id.to_owned(),
            QueuedEdit {
                original: msg,
                state: "editing".into(),
                result: Value::Null,
            },
        );
        self.revision += 1;
        Ok(json!({
            "ok": true,
            "editId": edit_id,
            "state": "editing",
            "message": view,
        }))
    }

    pub(crate) fn commit_edit(&mut self, edit_id: &str, payload: &Value) -> Result<Value, ApiError> {
        let Some(edit) = self.edits.get_mut(edit_id) else {
            return Err(ApiError::new(404, "队列编辑会话已失效，请重新编辑"));
        };
        if edit.state != "editing" {
            return Ok(edit.result.clone());
        }
        let mut updated = edit.original.clone();
        if let Some(prompt) = payload["prompt"].as_str() {
            updated.payload["prompt"] = json!(prompt);
        }
        if let Some(images) = payload.get("images").filter(|v| v.is_array()) {
            updated.payload["images"] = images.clone();
        }
        updated.state = "queued".into();
        updated.reason.clear();
        let view = updated.view(self.paused);
        self.messages.insert(0, updated);
        let result = json!({ "ok": true, "editId": edit_id, "state": "saved", "message": view });
        edit.state = "saved".into();
        edit.result = result.clone();
        self.compact_edits();
        self.revision += 1;
        Ok(result)
    }

    pub(crate) fn cancel_edit(&mut self, edit_id: &str) -> Result<Value, ApiError> {
        let Some(edit) = self.edits.get_mut(edit_id) else {
            return Ok(json!({ "ok": true, "editId": edit_id, "state": "cancelled" }));
        };
        let mut needs_compact = false;
        if edit.state == "editing" {
            let original = edit.original.clone();
            self.messages.insert(0, original);
            edit.state = "cancelled".into();
            edit.result = json!({ "ok": true, "editId": edit_id, "state": "cancelled" });
            needs_compact = true;
        }
        let result = edit.result.clone();
        if needs_compact {
            self.compact_edits();
            self.revision += 1;
        }
        Ok(result)
    }

    pub(crate) fn dismiss_edit(&mut self, edit_id: &str) -> Value {
        self.edits.remove(edit_id);
        self.revision += 1;
        json!({ "ok": true, "editId": edit_id })
    }

    fn compact_edits(&mut self) {
        let ended: Vec<String> = self
            .edits
            .iter()
            .filter(|(_, e)| e.state != "editing")
            .map(|(k, _)| k.clone())
            .collect();
        if ended.len() > 64 {
            for key in ended.into_iter().take(self.edits.len().saturating_sub(64)) {
                self.edits.remove(&key);
            }
        }
    }

    pub(crate) fn snapshot(&self, thread_id: &str, epoch: &str) -> Value {
        let messages: Vec<Value> = self.messages.iter().map(|m| m.view(self.paused)).collect();
        let edits: Vec<Value> = self
            .edits
            .iter()
            .filter(|(_, e)| e.state == "editing")
            .map(|(id, e)| {
                json!({
                    "editId": id,
                    "messageId": e.original.id,
                    "state": e.state,
                    "message": e.original.view(self.paused),
                })
            })
            .collect();
        json!({
            "ok": true,
            "threadId": thread_id,
            "queueEpoch": epoch,
            "queueRevision": self.revision,
            "queuedMessages": messages,
            "queuedEdits": edits,
            "queuePaused": self.paused,
        })
    }
}
