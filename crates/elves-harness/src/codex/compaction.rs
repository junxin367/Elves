use serde_json::{Value, json};

use super::store::Queued;
use super::{CodexHarness, error};
use crate::HarnessAuthorization;
use crate::codex_service::normalize_session_response;

impl CodexHarness {
    pub(super) async fn compact(
        &self,
        id: &str,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        if self.known_side(id).await {
            return self.compact_side(id, authorization).await;
        }
        self.ensure_loaded(id, authorization).await?;
        // 用户要求执行中的手动压缩进入同一队列；异步准备后核对实际状态，空闲时仍直接压缩。
        let identity = self
            .runtime
            .request(
                "thread/read",
                json!({"threadId":id,"includeTurns":false}),
                None,
            )
            .await?;
        normalize_session_response(&identity)?;
        if !self.send_is_running(id, &identity["thread"]).await? {
            return self.start_compaction(id, authorization).await;
        }
        let anchor = self.queue_anchor(id).await?;
        if anchor.0.is_none() && !anchor.1 {
            // 原轮次可能在读取与准入之间正常结束；重读后走直接压缩，不能留下没有依赖轮次的等待项。
            let latest = self
                .runtime
                .request(
                    "thread/read",
                    json!({"threadId":id,"includeTurns":false}),
                    None,
                )
                .await?;
            normalize_session_response(&latest)?;
            if !self.send_is_running(id, &latest["thread"]).await? {
                return self.start_compaction(id, authorization).await;
            }
            return Err(error(
                409,
                "当前执行轮次尚未确认，压缩未提交；请重新读取后再试",
            ));
        }
        let mut message = Queued::new(json!({"threadId":id}), authorization);
        message.compact = true;
        let mut store = self.store.lock().await;
        let permit = authorization.prepare_dispatch().await?;
        permit.commit(|| {
            store.commit(|data| {
                let queue = data.queues.entry(id.to_owned()).or_default();
                if queue.len() >= 100 {
                    return Err(error(429, "此会话的待执行操作已达上限"));
                }
                queue.push(message.clone());
                data.wait_queue(id, anchor.0, anchor.1);
                Ok(())
            })
        })?;
        drop(store);
        self.compaction_queued(id, &message.id).await
    }

    async fn compact_side(
        &self,
        id: &str,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let generation = self.side_generation(id).await?;
        let permit = authorization.prepare_dispatch().await?;
        let mut projection = self.projection.lock().await;
        let active = projection.active.get(id).cloned();
        let compaction = projection.compactions.get(id);
        let compacting = compaction.is_some();
        let anchor = compaction
            .and_then(|value| value["turnId"].as_str().map(str::to_owned))
            .or(active.clone());
        let side = projection.sides.threads.get_mut(id).unwrap();
        if side.closed
            || side.expired
            || side.closing
            || side.incomplete
            || side.generation != generation
        {
            return Err(error(410, "此侧边会话不能继续执行"));
        }
        if active.is_none() && !compacting {
            drop(projection);
            drop(permit);
            return self.start_compaction(id, authorization).await;
        }
        let mut message = Queued::new(json!({"threadId":id}), authorization);
        message.compact = true;
        // 执行状态与队列准入共用投影锁，不能漏掉恰好发生在点击压缩期间的侧边完成事件。
        permit.commit(|| {
            side.change_queue(|data| {
                let queue = data.queues.entry(id.to_owned()).or_default();
                if queue.len() >= 100 {
                    return Err(error(429, "此会话的待执行操作已达上限"));
                }
                queue.push(message.clone());
                data.wait_queue(id, anchor, compacting);
                Ok(())
            })
        })?;
        projection.side_changed(id);
        drop(projection);
        self.compaction_queued(id, &message.id).await
    }

    async fn compaction_queued(&self, id: &str, message_id: &str) -> anyhow::Result<Value> {
        self.queue_changed(id).await;
        let mut response = self.queue(id).await;
        response["messageId"] = json!(message_id);
        response["confirmed"] = json!(true);
        response["deliveryState"] = json!("queued");
        Ok(response)
    }

    pub(super) async fn start_compaction(
        &self,
        id: &str,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        if !self.known_side(id).await {
            self.ensure_loaded(id, authorization).await?;
        }
        let revision = self.projection.lock().await.compaction_revision(id);
        let result = if self.known_side(id).await {
            let generation = self.side_generation(id).await?;
            self.runtime
                .request_in_generation(
                    &generation,
                    "thread/compact/start",
                    json!({"threadId":id}),
                    authorization,
                )
                .await
        } else {
            self.runtime
                .request(
                    "thread/compact/start",
                    json!({"threadId":id}),
                    Some(authorization),
                )
                .await
        };
        result?;
        let mut projection = self.projection.lock().await;
        // 开始/完成事件可能早于 RPC 回执，不能将已完成压缩覆盖成 pending。
        if projection.compaction_revision(id) == revision {
            projection.compactions.insert(id.to_owned(),
                json!({"state":"pending","turnId":null,"itemId":"pending-manual-context-compaction"}));
        }
        let compaction = projection.compaction(id);
        let side = projection.sides.threads.contains_key(id);
        let turn = compaction["turnId"].as_str().map(str::to_owned);
        if side && compaction["state"] != "idle" {
            let side = projection.sides.threads.get_mut(id).unwrap();
            side.queue.wait_queue(id, turn.clone(), true);
            side.queue_revision += 1;
            projection.side_changed(id);
        }
        drop(projection);
        if !side && compaction["state"] != "idle" {
            self.remember_queue_wait(id, turn, true).await?;
        }
        Ok(json!({"ok":true,"threadId":id,"compaction":compaction}))
    }
}
