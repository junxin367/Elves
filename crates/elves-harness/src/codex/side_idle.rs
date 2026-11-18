//! 侧边闲置退订：页面租约、执行安全检查与原生加载生命周期，不保存或限制正文容量。
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::codex_service::{ServiceDispatchGuard, ServiceDispatchPermit};
use serde_json::{Value, json};
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::{CodexHarness, error, required};
use crate::HarnessAuthorization;

// 用户要求以闲置退订代替正文容量兜底：3 小时无查看且安全空闲才退订；
// 持久侧边另保留最近 10 个未活动加载项，临时侧边不按数量提前终止。
const IDLE_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
const MAX_INACTIVE_PERSISTENT: usize = 10;
// 页面每 30 秒续租，90 秒覆盖两个迟到周期；需与 side-panel.js 的心跳间隔同步核对。
const VIEW_LEASE: Duration = Duration::from_secs(90);
pub(super) const CHECK_INTERVAL: Duration = Duration::from_secs(30);

struct LoadedSide {
    generation: String,
    ephemeral: bool,
    inactive_since: Instant,
    failed: bool,
}

struct View {
    thread: String,
    sequence: u64,
    visible: bool,
    until: Instant,
}

#[derive(Default)]
pub(super) struct SideIdle {
    generation_index: u64,
    loaded: HashMap<String, LoadedSide>,
    views: HashMap<(String, String), View>,
}

struct IdleDispatch {
    state: Arc<Mutex<SideIdle>>,
    id: String,
    generation: String,
    since: Instant,
}

struct IdlePermit(OwnedMutexGuard<SideIdle>);

#[async_trait::async_trait]
impl ServiceDispatchGuard for IdleDispatch {
    async fn prepare_dispatch(&self) -> anyhow::Result<Box<dyn ServiceDispatchPermit>> {
        let state = self.state.clone().lock_owned().await;
        if state.viewed(&self.id, Instant::now())
            || state.loaded.get(&self.id).is_none_or(|side| {
                side.generation != self.generation
                    || side.inactive_since != self.since
                    || side.failed
            })
        {
            return Err(error(409, "侧边已重新查看或状态已变化，闲置退订未派发"));
        }
        Ok(Box::new(IdlePermit(state)))
    }
}

impl ServiceDispatchPermit for IdlePermit {
    fn commit(
        self: Box<Self>,
        dispatch: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
    ) -> anyhow::Result<()> {
        // 只在同步写入队列期间持有租约锁；等原生回执时必须释放，否则大量流式事件会堵住 RPC 回包。
        let _state = &self.0;
        dispatch()
    }
}

impl SideIdle {
    pub fn connected(&mut self, generation: &str, index: u64) {
        if index <= self.generation_index {
            return;
        }
        self.generation_index = index;
        self.loaded.retain(|_, side| side.generation == generation);
    }

    pub fn clear_loaded(&mut self) {
        self.loaded.clear();
    }

    pub fn loaded(&mut self, id: &str, generation: &str, ephemeral: bool) {
        self.loaded.insert(
            id.to_owned(),
            LoadedSide {
                generation: generation.to_owned(),
                ephemeral,
                inactive_since: Instant::now(),
                failed: false,
            },
        );
    }

    pub fn observe(&mut self, frame: &Value) {
        match frame["method"].as_str() {
            Some("harness/connected") => {
                self.connected(
                    frame["_generation"].as_str().unwrap_or(""),
                    frame["_generationIndex"].as_u64().unwrap_or(0),
                );
            }
            Some("harness/disconnected") => self
                .loaded
                .retain(|_, side| frame["_generation"] != side.generation),
            Some("thread/closed" | "thread/unloaded" | "thread/deleted") => {
                if let Some(id) = frame["params"]["threadId"].as_str() {
                    self.loaded.remove(id);
                }
            }
            Some("thread/status/changed") if frame["params"]["status"]["type"] == "notLoaded" => {
                if let Some(id) = frame["params"]["threadId"].as_str() {
                    self.loaded.remove(id);
                }
            }
            Some("turn/started" | "turn/completed") => {
                if let Some(side) = frame["params"]["threadId"]
                    .as_str()
                    .and_then(|id| self.loaded.get_mut(id))
                {
                    side.inactive_since = Instant::now();
                }
            }
            _ => {}
        }
    }

    fn viewed(&self, id: &str, now: Instant) -> bool {
        self.views
            .values()
            .any(|view| view.thread == id && view.visible && view.until > now)
    }

    fn presence(
        &mut self,
        device: &str,
        page: &str,
        id: &str,
        sequence: u64,
        visible: bool,
        now: Instant,
    ) {
        let key = (device.to_owned(), page.to_owned());
        if self
            .views
            .get(&key)
            .is_some_and(|view| view.sequence >= sequence)
        {
            return;
        }
        if let Some(previous) = self.views.get(&key).filter(|view| view.visible)
            && let Some(side) = self.loaded.get_mut(&previous.thread)
        {
            side.inactive_since = now;
        }
        if visible && let Some(side) = self.loaded.get_mut(id) {
            side.inactive_since = now + VIEW_LEASE;
        }
        // 隐藏记录也保留一个租约周期作为序号屏障，迟到的旧“显示”不能覆盖新隐藏/切换结果。
        self.views.insert(
            key,
            View {
                thread: id.to_owned(),
                sequence,
                visible,
                until: now + VIEW_LEASE,
            },
        );
    }
}

impl CodexHarness {
    pub(super) async fn check_side_idle_barrier(&self, id: &str) -> anyhow::Result<()> {
        if self
            .side_idle
            .lock()
            .await
            .loaded
            .get(id)
            .is_some_and(|side| side.failed)
        {
            return Err(error(
                409,
                "侧边闲置退订结果待核对；等待明确卸载事实或重新连接服务后再继续",
            ));
        }
        Ok(())
    }

    pub(super) async fn decorate_idle_release(&self, id: &str, thread: &mut Value) {
        if let Err(failure) = self.check_side_idle_barrier(id).await {
            thread["readOnly"] = json!(true);
            thread["readOnlyReason"] = json!(failure.to_string());
            thread["sideIdleReleaseUnknown"] = json!(true);
        }
    }

    pub(super) async fn side_presence(
        &self,
        payload: &Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        let id = required(payload, "threadId")?;
        let page = required(payload, "viewId")?;
        let sequence = payload["sequence"]
            .as_u64()
            .filter(|sequence| *sequence <= 9_007_199_254_740_991)
            .ok_or_else(|| error(400, "侧边页面序号无效"))?;
        let visible = payload["visible"]
            .as_bool()
            .ok_or_else(|| error(400, "侧边页面可见状态无效"))?;
        if page.len() > 120
            || !page
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(error(400, "侧边页面标识无效"));
        }
        // 只登记服务已核对的侧边；心跳不启动引擎、不恢复线程，设备只读也可声明正在查看。
        let known = {
            let projection = self.projection.lock().await;
            projection.persistent_sides.contains_key(id)
                || projection.sides.threads.contains_key(id)
        };
        if known {
            let device = format!(
                "{}:{}:{}",
                authorization.device.key_id,
                authorization.device.id,
                authorization.device.created_at_ms
            );
            self.side_idle.lock().await.presence(
                &device,
                page,
                id,
                sequence,
                visible,
                Instant::now(),
            );
        }
        Ok(json!({"ok":true,"threadId":id,"known":known}))
    }

    async fn side_safe_to_unload(&self, id: &str) -> bool {
        let projection = self.projection.lock().await;
        if projection.is_deleted(id)
            || projection.active.contains_key(id)
            || projection.compactions.contains_key(id)
            || !projection.requests_for(id).is_empty()
            || projection
                .threads
                .get(id)
                .is_some_and(|thread| thread["goal"]["status"] == "active")
        {
            return false;
        }
        if let Some(side) = projection.sides.threads.get(id) {
            if side.closed
                || side.expired
                || side.closing
                || side.incomplete
                || !safe_status(&side.metadata["status"])
                || side
                    .sent
                    .values()
                    .any(|(_, value)| value.is_null() || value["outcomeUnknown"] == true)
                || side
                    .queue
                    .queues
                    .get(id)
                    .is_some_and(|queue| !queue.is_empty())
                || side
                    .queue
                    .edits
                    .values()
                    .any(|edit| !matches!(edit.state.as_str(), "committed" | "canceled"))
            {
                return false;
            }
        } else if !projection.persistent_sides.contains_key(id) {
            return false;
        }
        drop(projection);
        let store = self.store.lock().await;
        !store.data.deletions.contains_key(id)
            && store.data.queues.get(id).is_none_or(Vec::is_empty)
            && !store.data.edits.values().any(|edit| {
                edit.thread_id == id && !matches!(edit.state.as_str(), "committed" | "canceled")
            })
            && store
                .data
                .persistent_sides
                .get(id)
                .is_none_or(|side| side.initialized)
    }

    pub(super) async fn release_idle_sides(&self) {
        let now = Instant::now();
        let records: Vec<_> = self
            .side_idle
            .lock()
            .await
            .loaded
            .iter()
            .map(|(id, side)| (id.clone(), side.generation.clone(), side.ephemeral))
            .collect();
        let mut candidates = Vec::new();
        for (id, generation, ephemeral) in records {
            let safe = self.side_safe_to_unload(&id).await;
            let mut state = self.side_idle.lock().await;
            if !safe || state.viewed(&id, now) {
                let active_until = state
                    .views
                    .values()
                    .filter(|view| view.thread == id && view.visible && view.until > now)
                    .map(|view| view.until)
                    .max()
                    .unwrap_or(now);
                if let Some(side) = state.loaded.get_mut(&id) {
                    side.inactive_since = active_until;
                }
                continue;
            }
            if let Some(side) = state.loaded.get(&id).filter(|side| !side.failed) {
                candidates.push((id, generation, ephemeral, side.inactive_since));
            }
        }
        candidates.sort_by_key(|(_, _, _, since)| *since);
        let mut overage = candidates
            .iter()
            .filter(|(_, _, ephemeral, _)| !ephemeral)
            .count()
            .saturating_sub(MAX_INACTIVE_PERSISTENT);
        for (id, generation, ephemeral, since) in candidates {
            let expired = now.saturating_duration_since(since) >= IDLE_TIMEOUT;
            if !expired && (ephemeral || overage == 0) {
                continue;
            }
            let lock = self.thread_lock(&id).await;
            let Ok(_serial) = lock.try_lock() else {
                continue;
            };
            if !self.side_safe_to_unload(&id).await {
                continue;
            }
            // 只退订本服务当前代次持有的原生线程；读取和派发均不能自动重连或接管外部 writer。
            if self.loaded.lock().await.get(&id) != Some(&generation) {
                continue;
            }
            let Some(identity) = self
                .runtime
                .idle_side_request(&generation, &id, None)
                .await
                .ok()
                .flatten()
            else {
                continue;
            };
            let raw = &identity["thread"];
            if raw["id"] != id || !safe_status(&raw["status"]) || raw["ephemeral"] != ephemeral {
                continue;
            }
            if !ephemeral && self.persistent_snapshot(raw).await.is_err() {
                continue;
            }
            if !ephemeral {
                match self.runtime.idle_side_goal(&generation, &id).await {
                    Ok(Some(goal))
                        if goal.get("goal") == Some(&Value::Null)
                            || matches!(
                                goal["goal"]["status"].as_str(),
                                Some("paused" | "complete" | "blocked")
                            ) => {}
                    _ => continue,
                }
            }
            if !self.side_safe_to_unload(&id).await {
                continue;
            }
            if !self.enabled.load(Ordering::Acquire) {
                continue;
            }
            let guard = IdleDispatch {
                state: self.side_idle.clone(),
                id: id.clone(),
                generation: generation.clone(),
                since,
            };
            let result = self
                .runtime
                .idle_side_request(&generation, &id, Some(&guard))
                .await;
            let canceled = result.as_ref().err().is_some_and(|failure| {
                failure
                    .downcast_ref::<crate::service_transport::DispatchFailure>()
                    .is_some_and(|failure| !failure.dispatched)
            });
            let confirmed = matches!(&result, Ok(Some(response)) if matches!(response["status"].as_str(),
                Some("unsubscribed" | "notLoaded" | "notSubscribed")));
            let mut state = self.side_idle.lock().await;
            if confirmed {
                state.loaded.remove(&id);
            } else if !canceled && let Some(side) = state.loaded.get_mut(&id) {
                // 未知退订不重放；只有新加载代次或明确原生卸载事实才能重新建立回收记录。
                side.failed = true;
            }
            drop(state);
            if canceled {
                continue;
            }
            if !confirmed {
                let _ = crate::diagnostic_log::append_diagnostic_log(
                    "remote_access.side_idle_unsubscribe_failed",
                    json!({"threadId":id,"error":format!("{result:?}")}),
                );
                continue;
            }
            if !ephemeral {
                overage = overage.saturating_sub(1);
            }
            let mut loaded = self.loaded.lock().await;
            if loaded.get(&id) == Some(&generation) {
                loaded.remove(&id);
            }
            drop(loaded);
            self.runtime.invalidate_read(&id).await;
            let mut projection = self.projection.lock().await;
            if projection.generation != generation {
                continue;
            }
            if ephemeral {
                if let Some(side) = projection.sides.threads.get_mut(&id) {
                    side.metadata["sideIdleReleased"] = json!(true);
                }
                projection.expire_side(&id);
            } else {
                if let Some(thread) = projection.threads.get_mut(&id) {
                    thread["status"] = json!({"type":"notLoaded"});
                }
                if let Some(parent) = projection.persistent_sides.get(&id).cloned() {
                    projection.push(json!({"method":"remote/relatedThreadsChanged","params":{"threadId":parent}}));
                }
            }
            projection.push(json!({"method":"remote/sideIdleReleased","params":{"threadId":id,"ephemeral":ephemeral}}));
        }
        self.side_idle
            .lock()
            .await
            .views
            .retain(|_, view| view.until > now);
    }

    #[cfg(test)]
    pub async fn age_side_for_test(&self, id: &str, seconds: u64) {
        if let Some(side) = self.side_idle.lock().await.loaded.get_mut(id) {
            side.inactive_since = Instant::now() - Duration::from_secs(seconds);
        }
    }

    #[cfg(test)]
    pub async fn release_idle_sides_for_test(&self) {
        self.release_idle_sides().await;
    }

    #[cfg(test)]
    pub async fn expire_side_views_for_test(&self) {
        for view in self.side_idle.lock().await.views.values_mut() {
            view.until = Instant::now() - Duration::from_secs(10802);
        }
    }
}

fn safe_status(status: &Value) -> bool {
    matches!(
        status.as_str().or_else(|| status["type"].as_str()),
        Some("idle" | "notLoaded" | "completed" | "failed" | "interrupted")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_service_side_idle_late_connection_does_not_lose_new_loaded_records() {
        let mut state = SideIdle::default();
        state.connected("new", 2);
        state.loaded("side", "new", false);
        state.observe(
            &json!({"method":"harness/connected","_generation":"old","_generationIndex":1}),
        );
        state.observe(&json!({"method":"harness/disconnected","_generation":"old"}));
        assert!(state.loaded.contains_key("side"));
        state.observe(
            &json!({"method":"harness/connected","_generation":"new","_generationIndex":2}),
        );
        assert!(state.loaded.contains_key("side"));
        state.connected("next", 3);
        assert!(state.loaded.is_empty());
    }
}
