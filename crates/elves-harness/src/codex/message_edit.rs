//! 历史消息编辑只经独立 app-server 回退并重发，不读取或驱动 Codex App。
use std::collections::HashSet;

use serde_json::{Value, json};

use super::commands::payload_hash;
use super::store::Queued;
use super::{CodexHarness, HarnessAuthorization, error, required, sent_key};

pub(super) fn history_head(latest: Option<&Value>) -> Value {
    latest
        .map(|turn| json!(format!("codex:{}", payload_hash(turn))))
        .unwrap_or(Value::Null)
}

pub(super) fn decorate_items(turn_id: &str, items: &mut [Value]) {
    // Codex 的编辑单位是轮次的原始输入；引导消息不能冒充该输入，否则会误删同轮之前的内容。
    let mut found = false;
    for item in items {
        if item["type"] == "userMessage" {
            let editable = !found && item["content"].is_array();
            found = true;
            item["messageMetadata"]["turnId"] = json!(turn_id);
            item["messageMetadata"]["messageId"] = item["id"].clone();
            item["messageMetadata"]["editable"] = json!(editable);
        }
    }
}

fn definite_failure(failure: &anyhow::Error) -> bool {
    failure
        .downcast_ref::<super::super::ApiError>()
        .is_some_and(|failure| !failure.outcome_unknown)
        || failure
            .downcast_ref::<crate::service_transport::DispatchFailure>()
            .is_some_and(|failure| !failure.dispatched)
}

impl CodexHarness {
    pub(super) async fn message_edit_ready(&self, id: &str, thread: &Value) -> bool {
        let status = &thread["status"];
        // 原生状态 systemError（上游或引擎报错后）同样不在执行，允许编辑重发；与退出判定口径一致。
        if !matches!(
            status.as_str().or_else(|| status["type"].as_str()),
            Some("idle" | "systemError" | "completed" | "failed" | "interrupted" | "notLoaded")
        ) {
            return false;
        }
        let projection = self.projection.lock().await;
        if projection.active.contains_key(id)
            || projection.compactions.contains_key(id)
            || !projection.requests_for(id).is_empty()
        {
            return false;
        }
        drop(projection);
        let store = self.store.lock().await;
        !store
            .data
            .queues
            .get(id)
            .is_some_and(|queue| !queue.is_empty())
            && !store.data.edits.values().any(|edit| {
                edit.thread_id == id && !matches!(edit.state.as_str(), "committed" | "canceled")
            })
    }

    pub(super) async fn edit_message(
        &self,
        id: &str,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let turn_id = required(payload, "turnId")?;
        let message_id = required(payload, "messageId")?;
        let mut followup = payload.clone();
        if followup["clientUserMessageId"]
            .as_str()
            .is_none_or(str::is_empty)
        {
            followup["clientUserMessageId"] = json!(uuid::Uuid::new_v4().to_string());
        }
        let key = sent_key(id, &followup);
        {
            let store = self.store.lock().await;
            if let Some(result) = store.data.sent.get(&key) {
                if store.data.sent_hashes.get(&key) != Some(&payload_hash(&followup)) {
                    return Err(error(409, "消息标识已用于其他内容"));
                }
                return Ok(result.clone());
            }
            if store
                .data
                .queues
                .get(id)
                .into_iter()
                .flatten()
                .any(|message| {
                    message.payload["clientUserMessageId"] == followup["clientUserMessageId"]
                })
            {
                return Ok(
                    json!({"ok":false,"code":409,"outcomeUnknown":true,"threadId":id,
                    "clientUserMessageId":followup["clientUserMessageId"],
                    "message":"历史编辑结果待核对，不会重复回退或发送"}),
                );
            }
        }
        // 恢复写入归属后再直读原生状态；分页历史不支持 includeTurns，不能只校验当前可见的三轮。
        self.ensure_loaded(id, authorization).await?;
        let identity = self
            .runtime
            .request(
                "thread/read",
                json!({"threadId":id,"includeTurns":false}),
                None,
            )
            .await?;
        crate::codex_service::normalize_thread_response(&identity)?;
        if !self.message_edit_ready(id, &identity["thread"]).await {
            return Err(error(
                409,
                "会话仍有执行、压缩、排队消息或待确认操作，暂不能编辑历史",
            ));
        }
        let mut cursor = Value::Null;
        let mut seen = HashSet::new();
        let mut removed = Vec::new();
        let mut latest = None;
        let target = loop {
            let page = self.runtime.request("thread/turns/list", json!({
                "threadId":id,"cursor":cursor,"limit":100,"itemsView":"notLoaded","sortDirection":"desc"
            }), None).await?;
            let turns = page["data"]
                .as_array()
                .ok_or_else(|| error(503, "历史轮次返回无效"))?;
            if latest.is_none() {
                latest = turns.first().cloned();
            }
            let mut target = None;
            for turn in turns {
                let current = required(turn, "id")?;
                if turn["status"] == "inProgress" {
                    return Err(error(409, "会话正在执行，暂不能编辑历史"));
                }
                removed.push(current.to_owned());
                if current == turn_id {
                    target = Some(turn.clone());
                    break;
                }
            }
            if let Some(target) = target {
                break target;
            }
            let Some(next) = page["nextCursor"]
                .as_str()
                .filter(|value| !value.is_empty())
            else {
                return Err(error(409, "历史消息已变化，请重新读取后编辑"));
            };
            if !seen.insert(next.to_owned()) {
                return Err(error(503, "历史轮次分页游标未前进"));
            }
            cursor = json!(next);
        };
        let latest_id = latest
            .as_ref()
            .and_then(|turn| turn["id"].as_str())
            .unwrap_or("");
        let latest = self
            .runtime
            .request(
                "thread/turns/list",
                json!({
                    "threadId":id,"limit":1,"itemsView":"full","sortDirection":"desc"
                }),
                None,
            )
            .await?;
        let latest = latest["data"].as_array().and_then(|turns| turns.first());
        if latest.and_then(|turn| turn["id"].as_str()) != Some(latest_id)
            || if payload["historyHead"].is_string() {
                payload["historyHead"] != history_head(latest)
            } else {
                // 旧网页没有历史头，只保留其原有的最后一轮编辑，不能隐式开放历史回退。
                latest_id != turn_id
            }
        {
            return Err(error(409, "会话历史已变化，请取消后重新编辑"));
        }
        let mut first = self
            .runtime
            .request(
                "thread/items/list",
                json!({
                    "threadId":id,"turnId":turn_id,"limit":40,"sortDirection":"asc"
                }),
                None,
            )
            .await?;
        let entries = first["data"]
            .as_array_mut()
            .ok_or_else(|| error(503, "原消息输入读取失败"))?;
        let message = entries
            .iter()
            .map(|entry| entry.get("item").unwrap_or(entry))
            .find(|item| item["type"] == "userMessage")
            .filter(|item| item["id"] == message_id)
            .ok_or_else(|| error(409, "原消息已变化或不是该轮原始输入"))?;
        // 包装项必须属于原轮次；修改文字仍保留图片、Skill 等全部非文字输入。
        if entries
            .iter()
            .any(|entry| entry["turnId"].is_string() && entry["turnId"] != target["id"])
        {
            return Err(error(503, "原消息输入归属无效"));
        }
        let mut input = message["content"]
            .as_array()
            .cloned()
            .ok_or_else(|| error(409, "原消息输入尚未完整加载"))?;
        if let Some(text) = input.iter_mut().find(|item| item["type"] == "text") {
            text["text"] = payload["prompt"].clone();
            text["text_elements"] = json!([]);
        } else {
            input.insert(
                0,
                json!({"type":"text","text":payload["prompt"],"text_elements":[]}),
            );
        }
        // 先持久保存同一客户端消息 ID，异常退出或新请求重试均不能再次回退历史。
        let mut intent = Queued::new(followup.clone(), authorization);
        intent.state = "direct-sending".into();
        let permit = authorization.prepare_dispatch().await?;
        {
            let mut store = self.store.lock().await;
            permit.commit(|| {
                store.commit(|data| {
                    data.queues
                        .entry(id.to_owned())
                        .or_default()
                        .push(intent.clone());
                    Ok(())
                })
            })?;
        }
        let reverted = self
            .runtime
            .request(
                "thread/revert",
                json!({"threadId":id,"beforeTurnId":turn_id}),
                Some(authorization),
            )
            .await;
        let result = match reverted {
            Ok(response) if response["thread"]["id"] == id => {
                let mut projection = self.projection.lock().await;
                projection.retire_history_turns(id, &removed);
                projection.push(json!({"method":"remote/threadReverted","params":{"threadId":id}}));
                drop(projection);
                self.start_input(id, &followup, input, authorization).await
            }
            Ok(_) => Err(anyhow::anyhow!(
                "历史回退回执不完整，请核对原操作；不会自动重发"
            )),
            Err(failure) => Err(failure),
        };
        self.store.lock().await.commit(|data| {
            let queue = data.queues.entry(id.to_owned()).or_default();
            match &result {
                Ok(response) => {
                    queue.retain(|message| message.id != intent.id);
                    data.sent.insert(key.clone(), response.clone());
                    data.sent_hashes
                        .insert(key.clone(), payload_hash(&followup));
                }
                Err(failure) if definite_failure(failure) => {
                    queue.retain(|message| message.id != intent.id);
                }
                Err(_) => {
                    if let Some(message) = queue.iter_mut().find(|message| message.id == intent.id)
                    {
                        message.state = "direct-unknown".into();
                    }
                }
            }
            Ok(())
        })?;
        result
    }
}
