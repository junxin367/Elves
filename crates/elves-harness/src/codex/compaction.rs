use std::path::PathBuf;
use std::sync::Weak;

use serde_json::{Value, json};

use super::store::{CompactionModelRestore, Queued};
use super::{CodexHarness, error};
use crate::HarnessAuthorization;
use crate::codex_service::normalize_session_response;
use crate::codex_service::ServiceDispatchGuard;

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

    pub(super) async fn codex_home_dir(&self) -> PathBuf {
        self.runtime
            .connection()
            .await
            .ok()
            .and_then(|connection| connection.codex_home.clone())
            .unwrap_or_else(|| {
                let settings = crate::settings::SettingsStore::default()
                    .load()
                    .unwrap_or_default();
                crate::codex_home::codex_home_dir_for_settings(&settings)
            })
    }

    async fn manual_compaction_cache_valid(&self, id: &str, current_model: &str) -> bool {
        let first_token = self.session_first_token(id).await;
        let attached = self.any_side(id).await;
        let home = self.codex_home_dir().await;
        let thread_id = id.to_owned();
        let metrics = tokio::task::spawn_blocking(move || {
            if attached {
                crate::session_metrics::codex_thread(&home, &thread_id, first_token, true)
            } else {
                crate::session_metrics::codex(&home, &thread_id, first_token)
            }
        })
        .await
        .unwrap_or(Value::Null);
        let metrics = crate::session_metrics::with_cache_expiry(metrics, id);
        crate::session_metrics::cache_is_valid(&metrics, current_model, crate::now_ms())
    }

    async fn reload_thread_with_model(
        &self,
        id: &str,
        model: &str,
        effort: Option<&str>,
        service_tier: Option<&str>,
        guard: Option<&dyn ServiceDispatchGuard>,
        remember_view: bool,
    ) -> anyhow::Result<()> {
        let connection = self.runtime.connection().await?;
        // Codex 0.171 在线程已加载且有订阅者时会忽略 thread/resume 的 model 覆盖，必须先卸载再按目标模型重连。
        let _ = self
            .runtime
            .request("thread/unsubscribe", json!({"threadId": id}), guard)
            .await;
        self.loaded.lock().await.remove(id);
        let identity = self
            .runtime
            .read("thread/read", json!({"threadId": id, "includeTurns": false}))
            .await?;
        let config = self
            .runtime
            .request(
                "config/read",
                json!({"cwd": identity["thread"]["cwd"], "includeLayers": true}),
                None,
            )
            .await?;
        let mut params = json!({"threadId": id, "excludeTurns": true, "model": model});
        super::super::instance_settings::apply_codex_compaction_default(&config, &mut params);
        if let Some(effort) = effort.filter(|value| !value.is_empty()) {
            params["config"]["model_reasoning_effort"] = json!(effort);
        }
        if let Some(tier) = service_tier.filter(|value| !value.is_empty()) {
            params["serviceTier"] = json!(tier);
        }
        if crate::codex_service::attached_parent_id(&identity["thread"]).is_some() {
            let side = self.persistent_snapshot(&identity["thread"]).await?;
            super::sides::apply_side_fork_permissions(&mut params, &side.permissions)?;
        }
        self.apply_auto_compact(&config, Some(model), &mut params)
            .await;
        let response = self
            .runtime
            .request("thread/resume", params, guard)
            .await?;
        let mut thread = normalize_session_response(&response)?;
        self.decorate_persistent_side(&response["thread"], &mut thread)
            .await;
        let persistent_side = thread["serviceSide"] == true;
        if remember_view {
            for key in ["model", "reasoningEffort", "serviceTier"] {
                if let Some(value) = response.get(key) {
                    thread[key] = value.clone();
                }
            }
            self.remember_settings(id, &response).await?;
            let mut loaded = self.loaded.lock().await;
            self.projection
                .lock()
                .await
                .threads
                .insert(id.to_owned(), thread);
            loaded.insert(id.to_owned(), connection.generation.clone());
        } else {
            // 临时切到压缩模型时不改写界面展示的会话模型与持久设置，压缩完后恢复原模型。
            self.loaded
                .lock()
                .await
                .insert(id.to_owned(), connection.generation.clone());
        }
        if persistent_side {
            self.side_idle
                .lock()
                .await
                .loaded(id, &connection.generation, false);
        }
        Ok(())
    }

    pub(super) async fn restore_compaction_model_if_needed(
        &self,
        id: &str,
        guard: Option<&dyn ServiceDispatchGuard>,
    ) -> anyhow::Result<()> {
        let pending = self
            .store
            .lock()
            .await
            .data
            .compaction_model_restores
            .get(id)
            .cloned();
        let Some(restore) = pending else {
            return Ok(());
        };
        if self.projection.lock().await.compactions.contains_key(id) {
            return Ok(());
        }
        self.reload_thread_with_model(
            id,
            &restore.original_model,
            restore.original_effort.as_deref(),
            restore.original_service_tier.as_deref(),
            guard,
            true,
        )
        .await?;
        self.store.lock().await.commit(|data| {
            data.compaction_model_restores.remove(id);
            Ok(())
        })?;
        self.projection
            .lock()
            .await
            .pending_compaction_models
            .remove(id);
        Ok(())
    }

    pub(super) async fn start_compaction(
        &self,
        id: &str,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let is_side = self.known_side(id).await;
        let mut switched_restore: Option<CompactionModelRestore> = None;
        if !is_side {
            self.restore_compaction_model_if_needed(id, Some(authorization))
                .await?;
            self.ensure_loaded(id, authorization).await?;
            // 用户要求仅在手动压缩时按策略临时切换压缩模型，执行中自动压缩不干预；
            // 临时侧边会话为 ephemeral 不可卸载重载，不执行切换。
            if let Some(controller) = self.controller.get().and_then(Weak::upgrade)
                && let Ok(pref) = crate::instance_settings::compaction_model_preference(
                    &controller.state_dir,
                    "codex:local",
                )
                && pref.model.is_some()
            {
                let (saved_model, saved_effort, saved_tier) = {
                    let store = self.store.lock().await;
                    let saved = store.data.settings.get(id);
                    (
                        saved
                            .and_then(|value| value["model"].as_str())
                            .map(str::to_owned),
                        saved
                            .and_then(|value| value["reasoningEffort"].as_str())
                            .map(str::to_owned),
                        saved
                            .and_then(|value| value["serviceTier"].as_str())
                            .map(str::to_owned),
                    )
                };
                let proj_model = self
                    .projection
                    .lock()
                    .await
                    .threads
                    .get(id)
                    .and_then(|thread| thread["model"].as_str())
                    .map(str::to_owned);
                let original_model = saved_model.or(proj_model).unwrap_or_default();
                let original_model = if original_model.is_empty() {
                    self.runtime
                        .read("thread/read", json!({"threadId": id, "includeTurns": false}))
                        .await
                        .ok()
                        .and_then(|read| read["thread"]["model"].as_str().map(str::to_owned))
                        .unwrap_or_default()
                } else {
                    original_model
                };
                if !original_model.is_empty() {
                    let cache_valid = self
                        .manual_compaction_cache_valid(id, &original_model)
                        .await;
                    if let Some(target_model) =
                        crate::instance_settings::resolve_manual_compaction_model(
                            &pref,
                            &original_model,
                            cache_valid,
                        )
                    {
                        let restore = CompactionModelRestore {
                            original_model: original_model.clone(),
                            original_effort: saved_effort,
                            original_service_tier: saved_tier,
                            compaction_model: target_model.clone(),
                        };
                        self.store.lock().await.commit(|data| {
                            data.compaction_model_restores
                                .insert(id.to_owned(), restore.clone());
                            Ok(())
                        })?;
                        if let Err(failure) = self
                            .reload_thread_with_model(
                                id,
                                &target_model,
                                None,
                                None,
                                Some(authorization),
                                false,
                            )
                            .await
                        {
                            let _ = self.store.lock().await.commit(|data| {
                                data.compaction_model_restores.remove(id);
                                Ok(())
                            });
                            let _ = self
                                .reload_thread_with_model(
                                    id,
                                    &restore.original_model,
                                    restore.original_effort.as_deref(),
                                    restore.original_service_tier.as_deref(),
                                    Some(authorization),
                                    true,
                                )
                                .await;
                            return Err(error(
                                503,
                                &format!("切换压缩模型 {target_model} 失败，压缩未发出：{failure}"),
                            ));
                        }
                        switched_restore = Some(restore);
                    }
                }
            }
        }
        let revision = {
            let mut p = self.projection.lock().await;
            // 切换成功且即将发出本次手动命令才冻结目标；错误回执、恢复或连接代次变化均清除。
            if let Some(restore) = &switched_restore {
                p.pending_compaction_models.insert(id.to_owned(), restore.compaction_model.clone());
            }
            p.compaction_revision(id)
        };
        let result = if is_side {
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
        if let Err(failure) = result {
            if let Some(restore) = switched_restore {
                self.projection
                    .lock()
                    .await
                    .pending_compaction_models
                    .remove(id);
                if self
                    .reload_thread_with_model(
                        id,
                        &restore.original_model,
                        restore.original_effort.as_deref(),
                        restore.original_service_tier.as_deref(),
                        Some(authorization),
                        true,
                    )
                    .await
                    .is_ok()
                {
                    let _ = self.store.lock().await.commit(|data| {
                        data.compaction_model_restores.remove(id);
                        Ok(())
                    });
                }
            }
            return Err(failure);
        }
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
