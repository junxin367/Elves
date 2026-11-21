//! Harness 的 Codex 原生执行适配器：维护会话归属、队列、编辑事务、审批和事件。
//! 用户要求 Codex 仅保留服务模式；不连接 Codex App，也不包含桌面 IPC、页面脚本或注入回退。
mod catalog;
mod commands;
mod compaction;
mod defaults;
mod deletion;
mod fast;
mod forks;
mod message_edit;
mod naming;
mod pins;
mod persistent_sides;
mod projection;
mod projects;
mod queue_gate;
mod read_cache;
mod reads;
mod runtime;
mod settings;
mod side_settings;
mod side_idle;
mod sides;
mod store;
mod subagents;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use anyhow::Context;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use super::execution;
use super::recent_priority::RecentPriority;
use super::{ApiError, HarnessAuthorization, HarnessController};
use crate::codex_service::ServiceDispatchGuard;
use catalog::Catalog;
use projection::Projection;
use runtime::Runtime;
use store::{Queued, Store};

/// 关停前等待在途请求自然返回的上限。prepare_controller_shutdown 已先设置 closing、
/// 停止队列派发，新请求不会进入；这里只给已派发的请求一个返回窗口，超时后按方法名列为阻塞原因。
/// 与下面两个原生查询上限合计约 5 + 5 + 20 = 30 秒，须小于 service.rs 的 SHUTDOWN_TIMEOUT（60 秒），
/// 否则关停请求超时被取消后 closing 标志无法复位。调整任一数值时同步核对三处。
pub(crate) const RELEASE_DRAIN_WAIT: Duration = Duration::from_secs(5);
const NATIVE_CHECK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const NATIVE_CHECK_BUDGET: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandoffStatus {
    // Runtime 尚未持有执行连接；它与“已有连接后断开”的 Unknown 必须分开处理。
    NoConnection,
    /// Elves 私有 app-server：Harness 退出或升级后，执行中的任务在服务里继续。
    PrivateServer,
    Stdio,
    Unknown,
}

impl HandoffStatus {
    /// 只有私有服务能让执行中的 Codex 在 Harness 退出后继续；stdio 子进程会随 Harness 结束。
    pub(crate) fn preserves_execution(self) -> bool {
        matches!(self, Self::PrivateServer)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoConnection => "no_connection",
            Self::PrivateServer => "private_server",
            Self::Stdio => "stdio",
            Self::Unknown => "unknown",
        }
    }
}

pub(super) struct CodexHarness {
    pub audit: Arc<execution::Audit>,
    pub recent_priority: Arc<RecentPriority>,
    runtime: Arc<Runtime>,
    catalog: Arc<Catalog>,
    projection: Arc<Mutex<Projection>>,
    store: Mutex<Store>,
    controller: OnceLock<Weak<HarnessController>>,
    receiver: Mutex<Option<mpsc::Receiver<Value>>>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    queue_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    side_idle_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    side_idle: Arc<Mutex<side_idle::SideIdle>>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    queue_recovery_after: Mutex<HashMap<String, std::time::Instant>>,
    loaded: Mutex<HashMap<String, String>>,
    /// 正在运行的命令（含后台终端），键为 `会话ID:条目ID`；见 store::OpenCommand。
    open_commands: Mutex<std::collections::BTreeMap<String, store::OpenCommand>>,
    /// 当前连接代次与对应的私有服务代次；只跟踪当前连接上的命令事件。
    connection_identity: Mutex<(String, Option<String>)>,
    pin_capability: Mutex<Option<(String, bool)>>,
    project_preferences: Mutex<Option<(std::path::PathBuf, projects::ProjectPreferences)>>,
    #[cfg(test)]
    project_codex_home: std::path::PathBuf,
    pub enabled: AtomicBool,
}

impl Drop for CodexHarness {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.get_mut().take() {
            worker.abort();
        }
        if let Some(worker) = self.queue_worker.get_mut().take() {
            worker.abort();
        }
        if let Some(worker) = self.side_idle_worker.get_mut().take() { worker.abort(); }
    }
}

impl CodexHarness {
    #[cfg(test)]
    pub async fn install_test_process(&self, process: crate::codex_service::RunningService) {
        self.runtime.install_test_process(process).await;
    }

    pub(crate) async fn handoff_status(&self) -> HandoffStatus {
        self.runtime.handoff_status().await
    }

    #[cfg(test)]
    pub async fn seed_test_history(&self, payload: Value) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.runtime.has_test_process(),
            "只允许向隔离测试引擎写入历史样例"
        );
        self.runtime
            .request("test/seedHistory", payload, None)
            .await?;
        Ok(())
    }
    #[cfg(test)]
    pub async fn start_test_turn(
        &self,
        payload: Value,
        authorization: &HarnessAuthorization,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(
            self.runtime.has_test_process(),
            "只允许向显式安装的隔离测试引擎启动原生轮次"
        );
        self.runtime
            .request("turn/start", payload, Some(authorization))
            .await
    }
    #[cfg(test)]
    pub async fn seed_test_subagents(&self, payload: Value) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.runtime.has_test_process(),
            "只允许配置隔离子智能体样例"
        );
        self.runtime
            .request("test/seedSubagents", payload, None)
            .await?;
        self.runtime.invalidate_read("thread/list").await;
        self.runtime.invalidate_read("thread/loaded/list").await;
        Ok(())
    }
    pub fn new(directory: &Path) -> anyhow::Result<Arc<Self>> {
        let (events, receiver) = mpsc::channel(512);
        let audit = Arc::new(execution::Audit::default());
        let store = Store::open(directory)?;
        let recent_priority = Arc::new(RecentPriority::new(directory));
        let mut initial_projection = Projection::with_recent_priority(recent_priority.clone());
        for (id, side) in &store.data.persistent_sides {
            initial_projection.persistent_sides.insert(id.clone(), side.parent.clone());
        }
        for (id, deletion) in &store.data.deletions {
            if deletion.state == "deleted" {
                initial_projection.forget_deleted_thread(id);
            }
        }
        // 上一个 Harness 交接下来的后台终端；接上同一代次的私有服务后才继续计入（见 observe_open_commands）。
        let inherited_commands = store
            .data
            .background_commands
            .iter()
            .map(|(key, command)| {
                let mut command = command.clone();
                command.inherited = true;
                (key.clone(), command)
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let projection = Arc::new(Mutex::new(initial_projection));
        Ok(Arc::new(Self {
            audit: audit.clone(),
            recent_priority,
            runtime: Arc::new(Runtime::new(events, audit.clone())),
            catalog: Arc::new(Catalog::default()),
            projection: projection.clone(),
            store: Mutex::new(store),
            controller: OnceLock::new(),
            receiver: Mutex::new(Some(receiver)),
            worker: Mutex::new(None),
            queue_worker: Mutex::new(None),
            side_idle_worker: Mutex::new(None),
            side_idle: Arc::new(Mutex::new(side_idle::SideIdle::default())),
            locks: Mutex::new(HashMap::new()),
            queue_recovery_after: Mutex::new(HashMap::new()),
            loaded: Mutex::new(HashMap::new()),
            open_commands: Mutex::new(inherited_commands),
            connection_identity: Mutex::new((String::new(), None)),
            pin_capability: Mutex::new(None),
            project_preferences: Mutex::new(None),
            #[cfg(test)]
            project_codex_home: directory.to_path_buf(),
            enabled: AtomicBool::new(false),
        }))
    }

    pub fn attach(&self, controller: Weak<HarnessController>) {
        let _ = self.controller.set(controller);
    }

    pub async fn configured_permissions(&self) -> anyhow::Result<Value> {
        // 只读实例的生效配置，不创建或恢复会话；配置保存后不能复用模型目录的 30 秒缓存。
        let response =
            execution::Scope::service(self.audit.clone(), "permission-config", &json!({}), None)
                .run(self.runtime.request(
                    "config/read",
                    json!({"cwd":null,"includeLayers":false}),
                    None,
                ))
                .await?;
        Ok(super::permissions::codex_config_preset(&response)?)
    }

    async fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut worker = self.worker.lock().await;
        if worker.is_none() {
            let mut receiver = self
                .receiver
                .lock()
                .await
                .take()
                .context("会话事件接收器已关闭")?;
            let weak = Arc::downgrade(self);
            *worker = Some(execution::spawn_service(
                self.audit.clone(),
                "engine-events",
                async move {
                    while let Some(frame) = receiver.recv().await {
                        let Some(service) = weak.upgrade() else { break };
                        if matches!(frame["method"].as_str(), Some("harness/connected" | "harness/disconnected"))
                            || frame["_generation"] == service.projection.lock().await.generation
                        {
                            service.side_idle.lock().await.observe(&frame);
                        }
                        service.observe_open_commands(&frame).await;
                        // 加载标记只属于当前独立引擎；收到卸载事件后清除旧代次记录。
                        let generation = frame["_generation"].as_str().unwrap_or("");
                        if frame["method"] == "thread/deleted"
                            && generation == service.projection.lock().await.generation
                        {
                            if let Some(id) = frame["params"]["threadId"]
                                .as_str()
                                .filter(|id| !id.is_empty())
                            {
                                // 先发布不可回退的删除事实，再按会话写锁清理；事件线程不能等待正在等 RPC 的写操作。
                                service.projection.lock().await.push(
                                    json!({"method":"thread/deleted","params":{"threadId":id}}),
                                );
                                service.catalog.observe(&frame).await;
                                let id = id.to_owned();
                                let cleanup = service.clone();
                                execution::spawn_service(
                                    service.audit.clone(),
                                    "delete-cleanup",
                                    async move {
                                        let lock = cleanup.thread_lock(&id).await;
                                        let _serial = lock.lock().await;
                                        cleanup.confirm_thread_deleted(&id).await;
                                    },
                                );
                            }
                            continue;
                        }
                        if frame["method"] == "thread/closed" || frame["method"] == "thread/unloaded"
                            || frame["method"] == "thread/status/changed"
                                && frame["params"]["status"]["type"] == "notLoaded"
                        {
                            if let Some(id) = frame["params"]["threadId"].as_str() {
                                let mut loaded = service.loaded.lock().await;
                                if loaded.get(id).is_some_and(|value| value == generation) {
                                    loaded.remove(id);
                                }
                            }
                        }
                        let paused = match service.observe_queue_lifecycle(&frame).await {
                            Ok(paused) => paused,
                            Err(failure) => {
                                let _ = crate::diagnostic_log::append_diagnostic_log(
                                    "remote_access.queue_completion_check_failed",
                                    json!({"error":format!("{failure:#}")}),
                                );
                                false
                            }
                        };
                        let affected = frame["params"]["threadId"].as_str().map(str::to_owned);
                        service.catalog.observe(&frame).await;
                        service.projection.lock().await.ingest(frame);
                        if paused && let Some(id) = affected {
                            service.queue_changed(&id).await;
                        }
                    }
                },
            ));
            let weak = Arc::downgrade(self);
            *self.queue_worker.lock().await = Some(execution::spawn_service(
                self.audit.clone(),
                "queue-worker",
                async move {
                    let mut tick = tokio::time::interval(Duration::from_millis(750));
                    loop {
                        tick.tick().await;
                        let Some(service) = weak.upgrade() else { break };
                        // 派发与接收独立运行，长请求不会堵住流式输出或审批事件。
                        if service.enabled.load(Ordering::Acquire) {
                            service.drain_queues().await;
                        }
                    }
                },
            ));
            let weak = Arc::downgrade(self);
            *self.side_idle_worker.lock().await = Some(execution::spawn_service(
                self.audit.clone(), "side-idle-release", async move {
                    let mut tick = tokio::time::interval(side_idle::CHECK_INTERVAL);
                    loop {
                        tick.tick().await;
                        let Some(service) = weak.upgrade() else { break };
                        if service.enabled.load(Ordering::Acquire) { service.release_idle_sides().await; }
                    }
                },
            ));
        }
        drop(worker);
        let connection = self.runtime.connection().await?;
        // start 可先于排队的连接事件完成；按单调代次登记，旧连接事件不能清掉新侧边的回收记录。
        self.side_idle.lock().await.connected(&connection.generation, connection.generation_index);
        self.projection
            .lock()
            .await
            .ingest(json!({"method":"harness/connected",
            "_generation":connection.generation,"_generationIndex":connection.generation_index}));
        Ok(())
    }

    #[cfg(test)]
    pub async fn prepare(self: &Arc<Self>) -> anyhow::Result<()> {
        execution::Scope::service(self.audit.clone(), "prepare", &json!({}), None)
            .run(self.start())
            .await
    }

    pub async fn prewarm(self: &Arc<Self>) -> anyhow::Result<()> {
        // 用户要求 Codex 仅使用服务模式；预热只读目录，不改变会话归属或启动任务。
        execution::Scope::service(self.audit.clone(), "prewarm", &json!({}), None)
            .run(async {
                self.start().await?;
                let model_params = json!({});
                let (sidebar, models) = tokio::join!(self.sidebar(), self.models(&model_params));
                sidebar?;
                models?;
                Ok(())
            })
            .await
    }

    pub async fn prewarm_for_listener(self: &Arc<Self>) -> anyhow::Result<()> {
        // 普通监听器单测不能因自动预热访问本机真实会话；启动预热测试显式注入隔离引擎。
        #[cfg(test)]
        if !self.runtime.has_test_process() {
            return Ok(());
        }
        self.prewarm().await
    }

    pub async fn stop_preparation(&self) {
        // 停止远程服务时取消目录预热，释放仅用于预热的引擎；
        // 已启用的服务任务保持原生命周期，不能因预热清理中断已有工作。
        if !self.enabled.load(Ordering::Acquire) && self.idle().await {
            self.release().await;
        } else {
            self.catalog.reset().await;
        }
    }

    pub async fn session_first_token(&self, thread_id: &str) -> Option<(String, u64)> {
        self.projection.lock().await.session_first_token(thread_id)
    }

    pub async fn side_session_metrics(&self, thread_id: &str) -> Option<Value> {
        self.projection.lock().await.side_session_metrics(thread_id)
    }

    /// 引擎内部释放（stop_preparation）沿用比退出更保守的口径：除退出阻塞原因外，
    /// 仍由当前引擎承载的临时侧边即使空闲也保留引擎，避免远程开关变化时丢掉正在查看的侧边上下文。
    /// 引擎重连后侧边只标记 expired（closed 仍为 false），它已不在当前引擎内，不能继续阻止释放。
    pub async fn idle(&self) -> bool {
        self.release_blockers(Duration::ZERO).await.is_empty()
            && self
                .projection
                .lock()
                .await
                .sides
                .threads
                .values()
                .all(|side| side.closed || side.expired)
    }

    /// 用户要求（2026-11-21）：安装和托盘退出只能被“真实执行中”或“仍在途的写操作”阻塞，
    /// 并逐条给出原因，不能再把所有情况折叠成 codex_active。判定口径：
    /// 1. 执行证据只认本 Harness 原生引擎里“已加载”线程的原生 ThreadStatus（thread/loaded/list
    ///    + thread/read）。未加载的线程不可能在本引擎内执行；投影里由 markRead、token 用量、
    ///    设置更新等事件补出的条目没有原生 status，不能当成执行中——此前正是这类条目让安装器
    ///    在没有任何任务执行时一直返回 codex_active。
    /// 2. 原生状态未知、查询失败、引擎连接断开但 Codex 子进程仍在 → 保守阻塞（fail-closed）。
    /// 3. 持久队列待派发/发送中/结果未知、编辑和删除未完成 → 阻塞（见 Data::release_blockers）。
    /// 4. 在途 RPC 最多等待 drain_wait；仍未返回的按方法名列出后阻塞。
    /// 修改口径时同步核对 prepare_controller_shutdown 的提示文案和 describe_release_blocker。
    pub async fn release_blockers(&self, drain_wait: Duration) -> Vec<Value> {
        let mut blockers = self.store.lock().await.data.release_blockers();
        match self.runtime.connection_state().await {
            runtime::ConnectionState::Alive => {}
            runtime::ConnectionState::NoConnection | runtime::ConnectionState::Lost => {
                // 没有存活连接时无法读取原生状态。私有服务断线：服务里的任务可能仍在执行；stdio 断线但
                // 本 Harness 名下仍有 Codex 子进程：同样无法确认。没有连接但上次留下的私有服务仍在运行：
                // 状态未知。以上都不能当成空闲；没有任何 Codex 进程时本引擎内不可能有任务执行。
                if self.runtime.private_server().await.is_some() {
                    blockers
                        .push(json!({"kind":"engine_disconnected","transport":"private_server"}));
                } else if codex_child_running() {
                    blockers.push(json!({"kind":"engine_disconnected"}));
                } else if crate::codex_server::running_for_settings().is_some() {
                    blockers.push(json!({"kind":"server_not_connected"}));
                }
                return blockers;
            }
        }
        let deadline = tokio::time::Instant::now() + drain_wait;
        let pending = loop {
            let pending = self.runtime.pending_methods().await;
            if pending.is_empty() || tokio::time::Instant::now() >= deadline {
                break pending;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        if !pending.is_empty() {
            blockers.push(json!({"kind":"request_in_flight","methods":pending}));
        }
        blockers.extend(self.native_execution_blockers().await);
        blockers
    }

    async fn native_execution_blockers(&self) -> Vec<Value> {
        // 沿用上游失败修复的放行条件：原生状态未知但投影已确认最后一轮以失败结束、
        // 且没有活动轮次/压缩/待处理请求时，不视为执行中。
        let terminal_failure_ids = {
            let projection = self.projection.lock().await;
            projection
                .threads
                .keys()
                .filter(|id| terminal_failure_from_projection(&projection, id))
                .cloned()
                .collect::<HashSet<_>>()
        };
        let unavailable = |method: &str, thread_id: Option<&str>, error: String| {
            json!({"kind":"native_state_unavailable","method":method,
                "threadId":thread_id,"error":error})
        };
        // 每个原生查询最多 5 秒、整体最多 20 秒；超时按“读取失败”保守阻塞，不能拖过关停预算。
        let budget = tokio::time::Instant::now() + NATIVE_CHECK_BUDGET;
        let query = |method: &'static str, params: Value| async move {
            let remaining = budget.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(
                remaining.min(NATIVE_CHECK_REQUEST_TIMEOUT),
                self.runtime.request(method, params, None),
            )
            .await
            {
                Ok(result) => result.map_err(|error| format!("{error:#}")),
                Err(_) => Err("查询超时".to_owned()),
            }
        };
        let mut loaded_ids = Vec::new();
        let mut seen = HashSet::new();
        let mut params = json!({"limit":100});
        let mut cursors = HashSet::new();
        loop {
            let page = match query("thread/loaded/list", params.clone()).await {
                Ok(page) => page,
                Err(error) => {
                    return vec![unavailable("thread/loaded/list", None, error)];
                }
            };
            let Some(rows) = page["data"].as_array() else {
                return vec![unavailable(
                    "thread/loaded/list",
                    None,
                    "返回格式无效".into(),
                )];
            };
            for row in rows {
                let Some(id) = row
                    .as_str()
                    .or_else(|| row["id"].as_str())
                    .or_else(|| row["threadId"].as_str())
                    .filter(|id| !id.is_empty())
                else {
                    return vec![unavailable(
                        "thread/loaded/list",
                        None,
                        "返回了无效会话 ID".into(),
                    )];
                };
                if seen.insert(id.to_owned()) {
                    loaded_ids.push(id.to_owned());
                }
            }
            let Some(cursor) = page["nextCursor"]
                .as_str()
                .filter(|cursor| !cursor.is_empty())
            else {
                break;
            };
            if !cursors.insert(cursor.to_owned()) {
                return vec![unavailable(
                    "thread/loaded/list",
                    None,
                    "分页游标重复".into(),
                )];
            }
            params["cursor"] = json!(cursor);
        }
        let mut blockers = Vec::new();
        // 会话已从引擎卸载时，它的后台终端随之结束，不再计入。
        let loaded_set: HashSet<&String> = loaded_ids.iter().collect();
        self.open_commands
            .lock()
            .await
            .retain(|_, command| loaded_set.contains(&command.thread_id));
        for id in loaded_ids {
            let thread =
                match query("thread/read", json!({"threadId":id,"includeTurns":false})).await {
                    Ok(response) => response["thread"].clone(),
                    Err(error) => {
                        blockers.push(unavailable("thread/read", Some(&id), error));
                        continue;
                    }
                };
            match native_execution(&thread) {
                NativeExecution::Running => {
                    // 等待审批或用户输入的轮次交接后无法再响应（重新接上时协议不会补发请求），单独标记。
                    let kind = if waiting_on_user(&thread["status"]) {
                        "turn_waiting"
                    } else {
                        "turn_active"
                    };
                    blockers.push(json!({"kind":kind,"threadId":id,
                        "status":thread["status"],"name":thread["name"]}));
                    continue;
                }
                NativeExecution::Unknown if !terminal_failure_ids.contains(&id) => {
                    blockers.push(json!({"kind":"native_status_unknown","threadId":id,
                        "status":thread["status"],"name":thread["name"]}));
                    continue;
                }
                _ => {}
            }
            // 轮次已结束：仍在运行的后台终端单独列出。交接来的记录收不到结束事件，先用条目状态核对。
            let inherited = self
                .open_commands
                .lock()
                .await
                .values()
                .any(|command| command.thread_id == id && command.inherited);
            if inherited
                && let Ok(response) =
                    query("thread/read", json!({"threadId":id,"includeTurns":true})).await
            {
                let finished: HashSet<String> = response["thread"]["turns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|turn| turn["items"].as_array())
                    .flatten()
                    .filter(|item| {
                        item["type"] == "commandExecution" && item["status"] != "inProgress"
                    })
                    .filter_map(|item| item["id"].as_str().map(str::to_owned))
                    .collect();
                self.open_commands.lock().await.retain(|_, command| {
                    !(command.thread_id == id
                        && command.inherited
                        && finished.contains(&command.item_id))
                });
            }
            let running: Vec<store::OpenCommand> = self
                .open_commands
                .lock()
                .await
                .values()
                .filter(|command| command.thread_id == id)
                .cloned()
                .collect();
            for command in running {
                blockers.push(json!({"kind":"background_terminal","threadId":id,
                    "name":thread["name"],"command":command.command,
                    "processId":command.process_id}));
            }
        }
        blockers
    }

    /// 跟踪命令执行条目：item/started 记录、item/completed 移除。exec_command 让出后命令仍在运行时，
    /// 这里是唯一能看到后台终端的地方。只接受当前连接的事件；重连到新服务或 stdio 时清空旧记录。
    async fn observe_open_commands(self: &Arc<Self>, frame: &Value) {
        let method = frame["method"].as_str().unwrap_or("");
        let generation = frame["_generation"].as_str().unwrap_or("");
        if method == "harness/connected" {
            let server = frame["_server"].as_str().map(str::to_owned);
            *self.connection_identity.lock().await = (generation.to_owned(), server.clone());
            self.open_commands
                .lock()
                .await
                .retain(|_, command| server.is_some() && command.server == server);
            if server.is_some() {
                let service = self.clone();
                let generation = generation.to_owned();
                execution::spawn_service(self.audit.clone(), "reattach-running", async move {
                    service.reattach_running_threads(&generation).await;
                });
            }
            return;
        }
        let current = self.connection_identity.lock().await.clone();
        if generation.is_empty() || generation != current.0 {
            return;
        }
        let params = &frame["params"];
        let Some(thread_id) = params["threadId"].as_str() else {
            return;
        };
        let item = &params["item"];
        match method {
            "item/started" | "item/completed" if item["type"] == "commandExecution" => {
                let Some(item_id) = item["id"].as_str() else {
                    return;
                };
                let key = format!("{thread_id}:{item_id}");
                let mut commands = self.open_commands.lock().await;
                if method == "item/completed" {
                    commands.remove(&key);
                } else if item["status"] == "inProgress" {
                    commands.insert(
                        key,
                        store::OpenCommand {
                            thread_id: thread_id.to_owned(),
                            item_id: item_id.to_owned(),
                            command: item["command"].as_str().unwrap_or("").to_owned(),
                            process_id: item["processId"].as_str().map(str::to_owned),
                            server: current.1.clone(),
                            inherited: false,
                        },
                    );
                }
            }
            "thread/closed" | "thread/unloaded" => {
                self.open_commands
                    .lock()
                    .await
                    .retain(|_, command| command.thread_id != thread_id);
            }
            "thread/status/changed" if params["status"]["type"] == "notLoaded" => {
                self.open_commands
                    .lock()
                    .await
                    .retain(|_, command| command.thread_id != thread_id);
            }
            _ => {}
        }
    }

    /// 用户要求（2026-11-21）：Harness 重启或升级后自动接上仍在私有服务里执行的会话。新连接默认只收到
    /// 会话状态变化，resume 后才会继续收到流式条目与轮次完成事件。这里只订阅，不改 loaded 记录：
    /// 之后的写入仍按 ensure_loaded 套用保存的会话设置。
    async fn reattach_running_threads(&self, generation: &str) {
        let Ok(page) = self
            .runtime
            .request("thread/loaded/list", json!({"limit":100}), None)
            .await
        else {
            return;
        };
        let ids: Vec<String> = page["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row.as_str().or_else(|| row["id"].as_str()))
            .map(str::to_owned)
            .collect();
        let with_commands: HashSet<String> = self
            .open_commands
            .lock()
            .await
            .values()
            .map(|command| command.thread_id.clone())
            .collect();
        for id in ids {
            if self.connection_identity.lock().await.0 != generation {
                return;
            }
            let running = match self
                .runtime
                .request(
                    "thread/read",
                    json!({"threadId":id,"includeTurns":false}),
                    None,
                )
                .await
            {
                Ok(read) => native_execution(&read["thread"]) == NativeExecution::Running,
                Err(_) => false,
            };
            if !running && !with_commands.contains(&id) {
                continue;
            }
            let result = self
                .runtime
                .request(
                    "thread/resume",
                    json!({"threadId":id,"excludeTurns":true}),
                    None,
                )
                .await;
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_server.reattached",
                json!({"threadId":id,"running":running,
                    "error":result.err().map(|error| format!("{error:#}"))}),
            );
        }
    }

    /// 当前（含已断开）连接对应的私有服务；stdio 或从未连接时为空。
    pub async fn private_server(&self) -> Option<crate::codex_server::PrivateServer> {
        self.runtime.private_server().await
    }

    /// 关停时保留私有服务：把该服务代次下仍在运行的命令交给下一个 Harness（只有接上同一代次才会沿用）。
    /// 保留的可能是当前连接的服务，也可能是上次留下、本次没连上的服务，所以由调用方指定代次。
    pub async fn hand_over_background_commands(&self, generation: &str) {
        let commands: std::collections::BTreeMap<String, store::OpenCommand> = self
            .open_commands
            .lock()
            .await
            .iter()
            .filter(|(_, command)| command.server.as_deref() == Some(generation))
            .map(|(key, command)| (key.clone(), command.clone()))
            .collect();
        if let Err(error) = self.store.lock().await.commit(|data| {
            data.background_commands = commands;
            Ok(())
        }) {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_server.handover_failed",
                json!({"error":format!("{error:#}")}),
            );
        }
    }

    /// 用户要求（2026-11-21）：没有执行中的会话时退出要连同 Codex 一起关闭。私有服务当前没有可用连接时
    /// （连接断开，或上次留下、本次尚未连上），退出前直接只读核对它，得到与已连接时同一口径的执行证据：
    /// 执行中或等待中的会话、仍在运行的后台终端（交接记录按条目状态核对）。返回空表示确认空闲，调用方
    /// 随后停止该服务；读取失败返回 Err，调用方保留服务并如实说明状态未知。
    pub async fn unconnected_server_blockers(
        &self,
        server: &crate::codex_server::PrivateServer,
    ) -> anyhow::Result<Vec<Value>> {
        let inherited: Vec<store::OpenCommand> = self
            .open_commands
            .lock()
            .await
            .values()
            .filter(|command| command.server.as_deref() == Some(server.generation.as_str()))
            .cloned()
            .collect();
        let with_turns: HashSet<String> = inherited
            .iter()
            .map(|command| command.thread_id.clone())
            .collect();
        let threads = crate::codex_server::read_loaded_threads(server, &with_turns).await?;
        let (blockers, ended) = server_thread_blockers(&threads, &inherited);
        if !ended.is_empty() {
            self.open_commands.lock().await.retain(|_, command| {
                command.server.as_deref() != Some(server.generation.as_str())
                    || !ended.contains(&(command.thread_id.clone(), command.item_id.clone()))
            });
        }
        Ok(blockers)
    }

    pub async fn release(&self) {
        self.catalog.reset().await;
        let server = self.runtime.private_server().await;
        self.runtime.shutdown().await;
        self.loaded.lock().await.clear();
        self.side_idle.lock().await.clear_loaded();
        self.open_commands.lock().await.clear();
        // 用户要求（2026-11-21）：确认空闲后释放引擎时，Elves 私有 Codex 服务一并停止，不在后台留下没有
        // 任务的进程。只有 stop_preparation 核对 idle 后、或关停判定确认没有可继续的任务时才会调用这里。
        if let Some(server) = server
            && let Err(error) = crate::codex_server::stop(&server)
        {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_server.stop_failed",
                json!({"processId":server.process_id,"error":format!("{error:#}")}),
            );
        }
        let mut store = self.store.lock().await;
        if !store.data.background_commands.is_empty() {
            let _ = store.commit(|data| {
                data.background_commands.clear();
                Ok(())
            });
        }
    }

    pub(crate) async fn thread_lock(&self, id: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    async fn ensure_loaded(
        &self,
        id: &str,
        guard: &dyn ServiceDispatchGuard,
    ) -> anyhow::Result<()> {
        let connection = self.runtime.connection().await?;
        if self.loaded.lock().await.get(id) == Some(&connection.generation) {
            return Ok(());
        }
        // 仅写操作才 resume；读取不抢占桌面会话，底层 writer lock 决定最终归属。
        let identity = self.runtime.read(
            "thread/read", json!({"threadId":id,"includeTurns":false}),
        ).await?;
        let config = self.runtime.request(
            "config/read", json!({"cwd":identity["thread"]["cwd"],"includeLayers":true}), None,
        ).await?;
        let mut params = json!({"threadId":id,"excludeTurns":true});
        super::instance_settings::apply_codex_compaction_default(&config, &mut params);
        if crate::codex_service::attached_parent_id(&identity["thread"]).is_some() {
            let side = self.persistent_snapshot(&identity["thread"]).await?;
            // 用户要求重启后沿用创建权限；resume 本身同样不能借用后来变化的全局权限。
            sides::apply_side_fork_permissions(&mut params, &side.permissions)?;
            if let Some(saved) = self.store.lock().await.data.settings.get(id) {
                if saved["model"].is_string() { params["model"] = saved["model"].clone(); }
                if saved["reasoningEffort"].is_string() {
                    params["config"]["model_reasoning_effort"] = saved["reasoningEffort"].clone();
                }
            }
        }
        let response = self
            .runtime
            .request(
                "thread/resume",
                params,
                Some(guard),
            )
            .await?;
        let mut thread = crate::codex_service::normalize_session_response(&response)?;
        self.decorate_persistent_side(&response["thread"], &mut thread).await;
        for key in ["model", "reasoningEffort", "serviceTier"] {
            if let Some(value) = response.get(key) {
                thread[key] = value.clone();
            }
        }
        self.remember_settings(id, &response).await?;
        let persistent_side = thread["serviceSide"] == true;
        // 用户要求远程会话独立于 Codex App；只登记本服务原生引擎的加载代次。
        let mut loaded = self.loaded.lock().await;
        self.projection
            .lock()
            .await
            .threads
            .insert(id.to_owned(), thread);
        loaded.insert(id.to_owned(), connection.generation.clone());
        if persistent_side {
            self.side_idle.lock().await.loaded(id, &connection.generation, false);
        }
        Ok(())
    }

    async fn remember_settings(&self, id: &str, settings: &Value) -> anyhow::Result<()> {
        let saved = self.store.lock().await.commit(|data| {
            let current = data
                .settings
                .entry(id.to_owned())
                .or_insert_with(|| json!({}));
            for key in ["model", "reasoningEffort", "serviceTier", "fastMode"] {
                if let Some(value) = settings.get(key) {
                    current[key] = value.clone();
                }
            }
            Ok(current.clone())
        })?;
        let mut projection = self.projection.lock().await;
        let thread = projection
            .threads
            .entry(id.to_owned())
            .or_insert_with(|| json!({}));
        for (key, value) in saved.as_object().into_iter().flatten() {
            thread[key] = value.clone();
        }
        Ok(())
    }

    async fn engine_loaded(&self, id: &str) -> anyhow::Result<bool> {
        let generation = self.runtime.connection().await?.generation.clone();
        Ok(self.loaded.lock().await.get(id) == Some(&generation))
    }

    pub async fn call(
        self: &Arc<Self>,
        action: &str,
        payload: Value,
        authorization: Option<&HarnessAuthorization>,
    ) -> anyhow::Result<Value> {
        execution::Scope::service(self.audit.clone(), action, &payload, authorization)
            .run(self.call_inner(action, payload, authorization))
            .await
    }

    async fn call_inner(
        self: &Arc<Self>,
        action: &str,
        payload: Value,
        authorization: Option<&HarnessAuthorization>,
    ) -> anyhow::Result<Value> {
        if let Some(authorization) = authorization {
            authorization.check().await?;
        }
        if action != "deleteThread"
            && let Some(id) = payload["threadId"].as_str()
            && self.projection.lock().await.is_deleted(id)
        {
            return Err(error(410, "会话已永久删除，请刷新会话列表。"));
        }
        // 用户要求过期后仍能查看和确认已读；已保留的侧边只处理本地内存，不等待失效引擎重连。
        let retained_side = if matches!(
            action,
            "thread" | "history" | "markRead" | "closeSideThread"
        ) {
            let projection = self.projection.lock().await;
            payload["threadId"]
                .as_str()
                .and_then(|id| projection.sides.threads.get(id))
                .is_some_and(|side| side.expired || side.closed)
        } else {
            false
        };
        let retained_deletion = action == "deleteThread"
            && self
                .store
                .lock()
                .await
                .data
                .deletions
                .contains_key(payload["threadId"].as_str().unwrap_or(""));
        let local_project = matches!(action, "workspaceContext" | "pinProject")
            && if let Some(id) = payload["projectId"].as_str() {
                self.local_project_context(id).await.is_some()
            } else {
                false
            };
        if !retained_side && !retained_deletion && !local_project && action != "sidePresence" {
            self.start().await.map_err(|failure| {
                if action == "deleteThread" {
                    deletion::delete_preflight_failure(failure)
                } else {
                    failure
                }
            })?;
        }
        let result = match action {
            "sidePresence" => self.side_presence(&payload,
                authorization.context("侧边页面状态缺少设备授权")?).await,
            "events" => {
                self.runtime.check_health().await?;
                // 只通过独立引擎刷新目录；Catalog 自身合并请求并限制读取频率。
                self.catalog
                    .refresh_in_background(&self.runtime, &self.projection);
                Ok(self.projection.lock().await.events(&payload))
            }
            "notifications" => Ok(self.projection.lock().await.notifications(&payload)),
            "sidebar" => self.sidebar().await,
            "models" => self.models(&payload).await,
            "newSessionDefaults" => self
                .creation_defaults(&payload)
                .await
                .map(|(defaults, _)| defaults),
            "fast" => self.fast_state(&payload).await,
            "skills" => self.skills(&payload).await,
            "thread" => self.thread(&payload).await,
            "history" => self.history(&payload).await,
            "workspaceContext" => self.workspace_context(&payload).await,
            "queue" => Ok(self.queue(required(&payload, "threadId")?).await),
            "queueEdit" => self.queue_edit(&payload).await,
            "sendState" => self.send_state(&payload).await,
            "relatedThreads" => self.related_sides(&payload).await,
            _ => {
                let authorization = authorization.context("会话写操作缺少设备授权")?;
                // 祖先删除会同时锁住全部后代；先串行化删除再取会话锁，避免两个删除各持一端互相等待。
                let _deletion_serial = if action == "deleteThread" {
                    Some(self.thread_lock("$persistent-side-delete").await.lock_owned().await)
                } else { None };
                let id = payload["threadId"].as_str().unwrap_or("$workspace");
                let lock = self.thread_lock(id).await;
                let _serial = lock.lock().await;
                authorization.check().await?;
                self.command(action, payload, authorization).await
            }
        }?;
        // 删除已被原生确认后保留成功回执；HTTP 出口仍检查授权，不能将事后过期写成“未删除”。
        if let Some(authorization) = authorization
            && !(action == "deleteThread" && result["deleted"] == true)
        {
            authorization.check().await?;
        }
        Ok(result)
    }

    async fn queue(&self, id: &str) -> Value {
        let projection = self.projection.lock().await;
        if let Some(side) = projection.sides.threads.get(id) {
            return side.queue_snapshot();
        }
        let epoch = projection.epoch.clone();
        drop(projection);
        self.store.lock().await.queue(id, &epoch)
    }

    async fn queue_changed(&self, id: &str) {
        let snapshot = self.queue(id).await;
        if snapshot["queuedMessages"]
            .as_array()
            .is_none_or(Vec::is_empty)
        {
            self.queue_recovery_after.lock().await.remove(id);
        }
        self.projection
            .lock()
            .await
            .push(json!({"method":"thread/queue/changed","params":snapshot}));
    }

    async fn drain_queues(self: &Arc<Self>) {
        // 用户要求取回编辑后等同未发送草稿，不阻塞后续队列；原槽位仅供再次发送时恢复顺序。
        // 这里只跳过 editing，暂停、发送中或结果未知的消息仍保持原有等待边界。
        let mut candidates: Vec<_> = self
            .store
            .lock()
            .await
            .data
            .queues
            .iter()
            .filter(|(_, q)| {
                q.iter()
                    .find(|m| m.state != "editing")
                    .is_some_and(|m| m.state == "queued")
            })
            .map(|(id, _)| id.clone())
            .collect();
        candidates.extend(
            self.projection
                .lock()
                .await
                .sides
                .threads
                .iter()
                .filter(|(id, side)| {
                    !side.closed
                        && !side.expired
                        && !side.closing
                        && side
                            .queue
                            .queue_gates
                            .get(*id)
                            .is_some_and(|gate| gate.phase == "ready")
                        && side.queue.queues.get(*id).is_some_and(|queue| {
                            queue
                                .iter()
                                .find(|message| message.state != "editing")
                                .is_some_and(|message| message.state == "queued")
                        })
                })
                .map(|(id, _)| id.clone()),
        );
        for id in candidates {
            let lock = self.thread_lock(&id).await;
            let Ok(_serial) = lock.try_lock() else {
                continue;
            };
            if self.check_deletion_barrier(&id).await.is_err() {
                continue;
            }
            if self.projection.lock().await.active.contains_key(&id) {
                continue;
            }
            if self.projection.lock().await.compactions.contains_key(&id) {
                continue;
            }
            let side_message = {
                let projection = self.projection.lock().await;
                projection.sides.threads.get(&id).and_then(|side| {
                    side.queue
                        .queues
                        .get(&id)
                        .and_then(|queue| queue.iter().find(|message| message.state != "editing"))
                        .cloned()
                })
            };
            let message = if side_message.is_some() {
                side_message
            } else {
                self.store
                    .lock()
                    .await
                    .data
                    .queues
                    .get(&id)
                    .and_then(|q| q.iter().find(|m| m.state != "editing"))
                    .cloned()
            };
            let Some(message) = message.filter(|q| q.state == "queued") else {
                continue;
            };
            let result = self.dispatch_queued(&id, &message).await;
            if let Err(error) = result {
                let mut projection = self.projection.lock().await;
                if let Some(side) = projection.sides.threads.get_mut(&id)
                    && side
                        .queue
                        .pause_queue(&id, "侧边队列发送失败，消息已暂停；请核对后手动重试")
                {
                    side.queue_revision += 1;
                    projection.side_changed(&id);
                }
                drop(projection);
                let _ = crate::diagnostic_log::append_diagnostic_log(
                    "remote_access.harness_queue_paused",
                    json!({"threadId":id,"messageId":message.id,"error":format!("{error:#}")}),
                );
            }
        }
    }

    async fn dispatch_queued(&self, id: &str, message: &Queued) -> anyhow::Result<()> {
        let controller = self
            .controller
            .get()
            .and_then(Weak::upgrade)
            .context("远程会话服务已停止")?;
        let device = controller
            .resolve_device(&message.device_id, Some(&message.key_id))
            .await
            .context("队列消息的设备授权已撤销")?;
        let mut authorization = controller.authorization(
            device,
            Some(if message.compact { "compact" } else { "send" }),
        );
        authorization.request_id = message.request_id.clone();
        execution::Scope::new(
            self.audit.clone(),
            "queue-dispatch",
            &json!({"threadId":id}),
            Some(&authorization),
        )
        .recorded(async {
            self.dispatch_queued_inner(id, message, &authorization, false)
                .await?;
            Ok(json!({"ok":true}))
        })
        .await
        .map(|_| ())
    }

    async fn dispatch_queued_inner(
        &self,
        id: &str,
        message: &Queued,
        authorization: &HarnessAuthorization,
        manual: bool,
    ) -> anyhow::Result<()> {
        authorization.check().await?;
        self.check_deletion_barrier(id).await?;
        if self.known_side(id).await {
            return self
                .dispatch_side_queued(id, message, authorization, manual)
                .await;
        }
        // 自动续发必须核对真实执行状态；重连后内存中没有 active 不代表上一轮正常完成。
        let identity = self
            .runtime
            .request(
                "thread/read",
                json!({"threadId":id,"includeTurns":false}),
                None,
            )
            .await?;
        crate::codex_service::normalize_session_response(&identity)?;
        if crate::codex_service::attached_parent_id(&identity["thread"]).is_some() {
            self.persistent_snapshot(&identity["thread"]).await?;
        }
        if self.send_is_running(id, &identity["thread"]).await? {
            let (turn, compacting) = self.queue_anchor(id).await?;
            self.remember_queue_wait(id, turn, compacting).await?;
            return Ok(());
        }
        if !manual
            && !self
                .queue_completion_confirmed(id, &identity, message.created_at)
                .await?
        {
            return Ok(());
        }
        {
            let projection = self.projection.lock().await;
            if projection.active.contains_key(id) || projection.compactions.contains_key(id) {
                return Ok(());
            }
        }
        self.store.lock().await.commit(|data| {
            anyhow::ensure!(
                manual
                    || data
                        .queue_gates
                        .get(id)
                        .is_some_and(|gate| gate.phase == "ready"),
                "上一轮完成状态已变化，队列继续等待"
            );
            let queued = data
                .queues
                .get_mut(id)
                .and_then(|q| {
                    q.iter_mut().find(|q| {
                        q.id == message.id && (q.state == "queued" || manual && q.state == "paused")
                    })
                })
                .context("队列消息已变化")?;
            queued.state = "sending".into();
            Ok(())
        })?;
        let result = if message.compact {
            self.start_compaction(id, authorization).await
        } else {
            self.start_turn(id, &message.payload, authorization).await
        };
        self.store.lock().await.commit(|data| {
            match &result {
                Ok(response) => {
                    data.sent
                        .insert(sent_key(id, &message.payload), response.clone());
                    data.sent_hashes.insert(
                        sent_key(id, &message.payload),
                        commands::payload_hash(&message.payload),
                    );
                    if let Some(queue) = data.queues.get_mut(id) {
                        queue.retain(|q| q.id != message.id);
                    }
                }
                Err(error) => {
                    if let Some(queued) = data
                        .queues
                        .get_mut(id)
                        .and_then(|q| q.iter_mut().find(|q| q.id == message.id))
                    {
                        queued.state = if error
                            .downcast_ref::<ApiError>()
                            .is_some_and(|error| !error.outcome_unknown)
                        {
                            "paused"
                        } else {
                            "unknown"
                        }
                        .into();
                    }
                    if message.compact {
                        // 压缩失败或结果未知时暂停整个后续队列；删除失败项也不能让后面的消息自行续发。
                        data.pause_queue(id, "压缩未确认完成，后续操作已暂停；请核对后手动重试");
                    }
                }
            }
            Ok(())
        })?;
        self.queue_changed(id).await;
        result.map(|_| ())
    }

    fn accept_queued_conversation_naming(
        &self,
        payload: &Value,
        result: &Value,
        authorization: &HarnessAuthorization,
    ) {
        let Some(controller) = self.controller.get().and_then(Weak::upgrade) else {
            return;
        };
        controller
            .conversation_naming
            .accepted("send", payload, result, &authorization.device.id);
    }
}

pub(super) fn terminal_failure_outcome(thread: &Value) -> bool {
    matches!(
        thread["lastTurnOutcome"]["status"].as_str(),
        Some("failed" | "rateLimit")
    ) || matches!(
        thread["lastTurnOutcome"]["kind"].as_str(),
        Some("failed" | "rateLimit")
    )
}

fn terminal_failure_from_projection(projection: &Projection, id: &str) -> bool {
    let Some(thread) = projection.threads.get(id) else {
        return false;
    };
    terminal_failure_outcome(thread)
        && !projection.active.contains_key(id)
        && !projection.compactions.contains_key(id)
        && projection.requests_for(id).is_empty()
}

#[derive(Debug, PartialEq, Eq)]
enum NativeExecution {
    NotRunning,
    Running,
    Unknown,
}

/// 按 Codex app-server 协议的 ThreadStatus 判断已加载线程是否正在本引擎内执行：
/// notLoaded / idle / systemError 表示当前没有轮次在跑（systemError 是失败后的终态，
/// 不能当成执行中）；active（含 waitingOnApproval / waitingOnUserInput）表示执行中。
/// 兼容旧引擎的 completed/failed/interrupted/stopped 与 inProgress/running/waiting 写法；
/// 缺失或无法识别的状态返回 Unknown，由调用方保守阻塞。
fn native_execution(thread: &Value) -> NativeExecution {
    let status = &thread["status"];
    match status.as_str().or_else(|| status["type"].as_str()) {
        Some(
            "notLoaded" | "idle" | "systemError" | "completed" | "failed" | "interrupted"
            | "stopped",
        ) => NativeExecution::NotRunning,
        Some("active" | "inProgress" | "running" | "waiting") => NativeExecution::Running,
        _ => NativeExecution::Unknown,
    }
}

/// 把直接读到的私有服务会话转换成关停原因（口径同 native_execution_blockers）：执行中或等待中的会话、
/// 状态无法识别的会话，以及轮次已结束但交接记录显示仍在运行的后台终端。第二个返回值是已结束或会话已
/// 卸载的交接记录（会话 ID、条目 ID），调用方据此清理。
fn server_thread_blockers(
    threads: &[Value],
    inherited: &[store::OpenCommand],
) -> (Vec<Value>, HashSet<(String, String)>) {
    let mut blockers = Vec::new();
    let mut settled = HashSet::new();
    let mut finished = HashSet::new();
    for thread in threads {
        let id = thread["id"].as_str().unwrap_or_default();
        match native_execution(thread) {
            NativeExecution::Running => {
                let kind = if waiting_on_user(&thread["status"]) {
                    "turn_waiting"
                } else {
                    "turn_active"
                };
                blockers.push(json!({"kind":kind,"threadId":id,"status":thread["status"],
                    "name":thread["name"],"transport":"private_server"}));
            }
            NativeExecution::Unknown => {
                blockers.push(json!({"kind":"native_status_unknown","threadId":id,
                    "status":thread["status"],"name":thread["name"],"transport":"private_server"}));
            }
            NativeExecution::NotRunning => {
                settled.insert(id.to_owned());
            }
        }
        for item in thread["turns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|turn| turn["items"].as_array())
            .flatten()
            .filter(|item| item["type"] == "commandExecution" && item["status"] != "inProgress")
        {
            if let Some(item_id) = item["id"].as_str() {
                finished.insert((id.to_owned(), item_id.to_owned()));
            }
        }
    }
    let loaded: HashSet<&str> = threads
        .iter()
        .filter_map(|thread| thread["id"].as_str())
        .collect();
    let mut ended = HashSet::new();
    for command in inherited {
        let key = (command.thread_id.clone(), command.item_id.clone());
        if !loaded.contains(command.thread_id.as_str()) || finished.contains(&key) {
            ended.insert(key);
        } else if settled.contains(&command.thread_id) {
            let name = threads
                .iter()
                .find(|thread| thread["id"] == command.thread_id.as_str())
                .map(|thread| thread["name"].clone())
                .unwrap_or(Value::Null);
            blockers.push(
                json!({"kind":"background_terminal","threadId":command.thread_id,
                "name":name,"command":command.command,"processId":command.process_id,
                "transport":"private_server"}),
            );
        }
    }
    (blockers, ended)
}

/// 用户要求（2026-11-21，第二次确认）：托盘退出和安装升级一律放行，不能因 Codex 状态拦住；Codex 在执行
/// 就留在 Elves 私有服务里继续。关停时每条原因只决定处理方式和提示：
/// - Execution：执行中的轮次、后台终端、状态未知。私有服务保留并继续执行，下次启动自动接上；stdio 兜底
///   无法保留，随退出中断。
/// - Waiting：等待审批或用户输入。私有服务里会继续等待，但当前 Codex（0.167）重新连接后不会再发出这条
///   请求，重新打开 Elves 后只能停止这一轮再重新发送；stdio 下随退出中断。
/// - Saved：队列、编辑、删除未完成或请求未返回。状态已持久保存，Store::open 会把发送中转为待核对、
///   删除转为未知并保持保护、排队消息重新核对上一轮正常结束，下次启动后继续处理，不会重发。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownEffect {
    Execution,
    Waiting,
    Saved,
}

pub(crate) fn shutdown_effect(blocker: &Value) -> ShutdownEffect {
    match blocker["kind"].as_str() {
        Some("turn_waiting") => ShutdownEffect::Waiting,
        Some("queue_pending" | "edit_pending" | "deletion_pending" | "request_in_flight") => {
            ShutdownEffect::Saved
        }
        _ => ShutdownEffect::Execution,
    }
}

/// 一次退出或安装要如何处理 Codex：只分类和生成提示，从不拦截（见 shutdown_effect）。
#[derive(Debug, Default)]
pub(crate) struct ShutdownPlan {
    /// 留在私有服务里继续执行。
    pub continuing: Vec<Value>,
    /// 留在私有服务里继续等待审批或输入，重新连接后无法再回应。
    pub waiting: Vec<Value>,
    /// 没有运行在私有服务上，随退出中断。
    pub interrupted: Vec<Value>,
    /// 已持久保存，下次启动后继续处理。
    pub saved: Vec<Value>,
}

impl ShutdownPlan {
    /// runtime_preserves：当前连接本身就是私有服务。没有可用连接时，来自私有服务的原因（带
    /// transport=private_server，或上次留下的服务尚未连上）同样能留在服务里继续。
    pub fn new(blockers: Vec<Value>, runtime_preserves: bool) -> Self {
        let mut plan = Self::default();
        for blocker in blockers {
            let preserved = runtime_preserves
                || blocker["transport"] == "private_server"
                || blocker["kind"] == "server_not_connected";
            match (shutdown_effect(&blocker), preserved) {
                (ShutdownEffect::Saved, _) => plan.saved.push(blocker),
                (ShutdownEffect::Execution, true) => plan.continuing.push(blocker),
                (ShutdownEffect::Waiting, true) => plan.waiting.push(blocker),
                (_, false) => plan.interrupted.push(blocker),
            }
        }
        plan
    }

    /// 仍有任务在私有服务里执行或等待时保留服务，否则退出时连同私有服务一起停止。
    pub fn keeps_server(&self) -> bool {
        !self.continuing.is_empty() || !self.waiting.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.continuing.is_empty()
            && self.waiting.is_empty()
            && self.interrupted.is_empty()
            && self.saved.is_empty()
    }

    /// 托盘退出记录与安装日志使用的逐条说明；没有任何需要说明的任务时为空。
    pub fn message(&self, uninstall: bool) -> Option<String> {
        let continuing = if uninstall {
            "以下 Codex 任务会在后台执行完，结果保存在 Codex 会话记录里；卸载后不会再有 Elves 接上它们："
        } else {
            "以下 Codex 任务将在后台继续执行，下次启动 Elves 会自动接上："
        };
        let sections = [
            (continuing, &self.continuing),
            (
                "以下会话正在等待审批或你的输入：Codex 会在后台继续等待，但当前 Codex 版本在重新连接后不会再发出这条请求，重新打开 Elves 后需要停止这一轮并重新发送：",
                &self.waiting,
            ),
            (
                "当前 Codex 没有运行在 Elves 私有服务上，以下任务无法在后台继续，已随退出中断：",
                &self.interrupted,
            ),
            (
                "以下未完成的操作已保存，下次启动 Elves 后继续处理（排队消息在确认上一轮正常结束后发送，结果未知的消息按会话记录核对，不会重发）：",
                &self.saved,
            ),
        ];
        let lines: Vec<String> = sections
            .into_iter()
            .filter(|(_, items)| !items.is_empty())
            .flat_map(|(title, items)| {
                std::iter::once(title.to_owned()).chain(
                    items
                        .iter()
                        .map(|blocker| format!("- {}", describe_release_blocker(blocker))),
                )
            })
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

fn waiting_on_user(status: &Value) -> bool {
    status["activeFlags"].as_array().is_some_and(|flags| {
        flags
            .iter()
            .any(|flag| flag == "waitingOnApproval" || flag == "waitingOnUserInput")
    })
}

/// 把 release_blockers 的单条原因转成面向用户的中文说明，供托盘退出弹窗和安装器日志逐行展示。
/// 新增阻塞类型时须同步补充这里的文案。
pub(crate) fn describe_release_blocker(blocker: &Value) -> String {
    let thread = || {
        let id = blocker["threadId"].as_str().unwrap_or("未知会话");
        match blocker["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            Some(name) => {
                let short: String = name.chars().take(24).collect();
                let ellipsis = if name.chars().count() > 24 { "…" } else { "" };
                format!("会话「{short}{ellipsis}」（{id}）")
            }
            None => format!("会话 {id}"),
        }
    };
    let list = |key: &str| {
        blocker[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("、")
    };
    match blocker["kind"].as_str().unwrap_or("") {
        "turn_active" => {
            let flags = list_flags(&blocker["status"]["activeFlags"]);
            if flags.is_empty() {
                format!("{}：Codex 正在执行", thread())
            } else {
                format!("{}：Codex 正在执行（{flags}）", thread())
            }
        }
        "turn_waiting" => {
            let reason = if blocker["status"]["activeFlags"]
                .as_array()
                .is_some_and(|flags| flags.iter().any(|flag| flag == "waitingOnApproval"))
            {
                "等待审批"
            } else {
                "等待你的输入"
            };
            format!("{}：Codex 正在{reason}", thread())
        }
        "background_terminal" => {
            let command: String = blocker["command"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            format!("{}：后台终端仍在运行：{command}", thread())
        }
        "native_status_unknown" => format!(
            "{}：原生状态无法识别（{}），按执行中处理",
            thread(),
            blocker["status"]
        ),
        "native_state_unavailable" => match blocker["threadId"].as_str() {
            Some(_) => format!(
                "{}：读取原生状态失败（{}），按执行中处理",
                thread(),
                blocker["error"].as_str().unwrap_or("未知错误")
            ),
            None => format!(
                "读取 Codex 已加载会话失败（{}），按执行中处理",
                blocker["error"].as_str().unwrap_or("未知错误")
            ),
        },
        "request_in_flight" => format!("请求尚未返回：{}", list("methods")),
        "queue_pending" => format!("{}：消息队列尚未发送完成（{}）", thread(), list("states")),
        "edit_pending" => format!(
            "{}：消息编辑尚未完成（{}）",
            thread(),
            blocker["state"].as_str().unwrap_or("未知")
        ),
        "deletion_pending" => format!(
            "{}：删除结果尚未确认（{}）",
            thread(),
            blocker["state"].as_str().unwrap_or("未知")
        ),
        "engine_disconnected" if blocker["transport"] == "private_server" => {
            "与 Elves 私有 Codex 服务的连接已断开，无法确认任务状态".into()
        }
        "engine_disconnected" => {
            "Codex 引擎连接已断开，但 Codex 子进程仍在运行，无法确认是否在执行".into()
        }
        "server_not_connected" => {
            "上次留下的 Elves 私有 Codex 服务仍在运行，本次尚未连接，任务状态未知".into()
        }
        other => format!("未分类的阻塞原因：{other}"),
    }
}

fn list_flags(flags: &Value) -> String {
    flags
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|flag| match flag {
            "waitingOnApproval" => "等待审批",
            "waitingOnUserInput" => "等待用户输入",
            other => other,
        })
        .collect::<Vec<_>>()
        .join("、")
}

fn codex_child_running() -> bool {
    #[cfg(windows)]
    {
        let current_process_id = std::process::id();
        let processes = elves_core::windows_integration::enumerate_processes();
        let mut parents = HashSet::from([current_process_id]);
        let mut changed = true;
        while changed {
            changed = false;
            for process in &processes {
                if !parents.contains(&process.parent_process_id) {
                    continue;
                }
                if process.exe_file.eq_ignore_ascii_case("codex.exe") {
                    return true;
                }
                if parents.insert(process.process_id) {
                    changed = true;
                }
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn error(code: u16, message: &str) -> anyhow::Error {
    ApiError::new(code, message).into()
}
fn required<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value[key]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| error(400, &format!("缺少 {key}")))
}
fn sent_key(id: &str, payload: &Value) -> String {
    format!(
        "{id}:{}",
        payload["clientUserMessageId"].as_str().unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_execution_follows_thread_status_protocol_and_fails_closed() {
        for status in [
            json!({"type":"idle"}),
            json!({"type":"notLoaded"}),
            json!({"type":"systemError"}),
        ] {
            assert_eq!(
                native_execution(&json!({"status":status})),
                NativeExecution::NotRunning,
                "{status}"
            );
        }
        assert_eq!(
            native_execution(&json!({"status":{"type":"active","activeFlags":[]}})),
            NativeExecution::Running
        );
        assert_eq!(
            native_execution(
                &json!({"status":{"type":"active","activeFlags":["waitingOnApproval"]}})
            ),
            NativeExecution::Running
        );
        assert_eq!(native_execution(&json!({})), NativeExecution::Unknown);
        assert_eq!(
            native_execution(&json!({"status":{"type":"unknown"}})),
            NativeExecution::Unknown
        );
    }

    #[test]
    fn release_blocker_descriptions_name_the_session_and_reason() {
        let line = describe_release_blocker(&json!({
            "kind":"turn_active","threadId":"thread-a","name":"修复安装器",
            "status":{"type":"active","activeFlags":["waitingOnApproval"]}
        }));
        assert!(
            line.contains("修复安装器") && line.contains("thread-a") && line.contains("等待审批")
        );
        let line = describe_release_blocker(&json!({
            "kind":"request_in_flight","methods":["thread/resume","turn/start"]
        }));
        assert!(line.contains("thread/resume、turn/start"));
        let line = describe_release_blocker(&json!({
            "kind":"queue_pending","threadId":"thread-b","states":["queued","sending"]
        }));
        assert!(line.contains("thread-b") && line.contains("queued、sending"));
        // 退出一律放行；每条原因只决定处理方式：执行类留在私有服务继续，等待审批单独提示，其余保存待续。
        let waiting = json!({"kind":"turn_waiting","threadId":"thread-c",
            "status":{"type":"active","activeFlags":["waitingOnUserInput"]}});
        assert!(describe_release_blocker(&waiting).contains("等待你的输入"));
        assert_eq!(shutdown_effect(&waiting), ShutdownEffect::Waiting);
        for kind in [
            "queue_pending",
            "edit_pending",
            "deletion_pending",
            "request_in_flight",
        ] {
            assert_eq!(
                shutdown_effect(&json!({"kind":kind})),
                ShutdownEffect::Saved,
                "{kind}"
            );
        }
        for kind in [
            "turn_active",
            "background_terminal",
            "native_status_unknown",
            "native_state_unavailable",
            "engine_disconnected",
            "server_not_connected",
        ] {
            assert_eq!(
                shutdown_effect(&json!({"kind":kind})),
                ShutdownEffect::Execution,
                "{kind}"
            );
        }
    }

    #[test]
    fn shutdown_plan_never_blocks_and_reports_each_effect() {
        let blockers = || {
            vec![
                json!({"kind":"turn_active","threadId":"run","status":{"type":"active","activeFlags":[]}}),
                json!({"kind":"background_terminal","threadId":"term","command":"npm run dev"}),
                json!({"kind":"turn_waiting","threadId":"ask",
                    "status":{"type":"active","activeFlags":["waitingOnApproval"]}}),
                json!({"kind":"queue_pending","threadId":"queue","states":["queued"]}),
            ]
        };
        // 私有服务：执行和后台终端继续，等待审批单独提示，排队消息保存待续，服务保留。
        let plan = ShutdownPlan::new(blockers(), true);
        assert_eq!(plan.continuing.len(), 2);
        assert_eq!(plan.waiting.len(), 1);
        assert_eq!(plan.saved.len(), 1);
        assert!(plan.interrupted.is_empty() && plan.keeps_server());
        let message = plan.message(false).unwrap();
        assert!(message.contains("后台继续执行") && message.contains("npm run dev"));
        assert!(message.contains("停止这一轮并重新发送") && message.contains("ask"));
        assert!(message.contains("已保存") && message.contains("queue"));
        assert!(
            plan.message(true)
                .unwrap()
                .contains("卸载后不会再有 Elves 接上")
        );
        // stdio：执行和等待都无法保留，如实说明已中断，不保留服务；排队消息照样保存。
        let plan = ShutdownPlan::new(blockers(), false);
        assert_eq!(plan.interrupted.len(), 3);
        assert_eq!(plan.saved.len(), 1);
        assert!(!plan.keeps_server());
        assert!(plan.message(false).unwrap().contains("已随退出中断"));
        // 空闲：没有任何说明，连同私有服务一起停止。
        let plan = ShutdownPlan::new(Vec::new(), true);
        assert!(plan.is_empty() && !plan.keeps_server() && plan.message(false).is_none());
        // 当前没有连上私有服务（stdio 或连接断开）时，带私有服务标记的原因仍能留在服务里继续。
        let plan = ShutdownPlan::new(
            vec![
                json!({"kind":"turn_active","threadId":"server","transport":"private_server"}),
                json!({"kind":"server_not_connected"}),
                json!({"kind":"turn_active","threadId":"stdio"}),
            ],
            false,
        );
        assert_eq!(plan.continuing.len(), 2);
        assert_eq!(plan.interrupted.len(), 1);
        assert!(plan.keeps_server());
    }

    #[test]
    fn unconnected_server_threads_map_to_execution_evidence() {
        let command = |thread: &str, item: &str| store::OpenCommand {
            thread_id: thread.into(),
            item_id: item.into(),
            command: format!("run {item}"),
            process_id: Some("61802".into()),
            server: Some("server-a".into()),
            inherited: true,
        };
        let threads = vec![
            json!({"id":"running","name":"执行中","status":{"type":"active","activeFlags":[]}}),
            json!({"id":"asking","status":{"type":"active","activeFlags":["waitingOnApproval"]}}),
            // 轮次已结束：仍在运行的命令不出现在 turns 里，已结束的以非 inProgress 状态出现。
            json!({"id":"idle","name":"空闲","status":{"type":"idle"},"turns":[{"items":[
                {"id":"done","type":"commandExecution","status":"completed"}]}]}),
        ];
        let inherited = vec![
            command("idle", "still-running"),
            command("idle", "done"),
            command("unloaded", "gone"),
        ];
        let (blockers, ended) = server_thread_blockers(&threads, &inherited);
        let kinds: Vec<&str> = blockers
            .iter()
            .map(|blocker| blocker["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["turn_active", "turn_waiting", "background_terminal"]
        );
        assert!(
            blockers
                .iter()
                .all(|blocker| blocker["transport"] == "private_server")
        );
        assert_eq!(blockers[2]["command"], "run still-running");
        assert_eq!(blockers[2]["name"], "空闲");
        assert!(ended.contains(&("idle".into(), "done".into())));
        assert!(ended.contains(&("unloaded".into(), "gone".into())));
        assert_eq!(ended.len(), 2);
        // 全部空闲且没有仍在运行的交接记录：确认空闲，调用方随后停止该服务。
        let (blockers, ended) =
            server_thread_blockers(&[json!({"id":"idle","status":{"type":"idle"}})], &[]);
        assert!(blockers.is_empty() && ended.is_empty());
    }

    const RELEASE_TEST_ENGINE: &str = r#"
const readline = require('readline');
const statuses = JSON.parse(process.argv[1]);
const input = readline.createInterface({input: process.stdin});
input.on('line', line => {
  const q = JSON.parse(line);
  if (q.id === undefined) return;
  let result;
  switch (q.method) {
    case 'initialize': result = {userAgent: 'release-test-engine'}; break;
    case 'thread/loaded/list': result = {data: Object.keys(statuses), nextCursor: null}; break;
    case 'thread/read': result = {thread: {id: q.params.threadId, name: 'name-' + q.params.threadId,
      status: statuses[q.params.threadId]}}; break;
    case 'test/hang': return;
    default:
      process.stdout.write(JSON.stringify({id: q.id, error: {code: -32601, message: 'unsupported'}}) + '\n');
      return;
  }
  process.stdout.write(JSON.stringify({id: q.id, result}) + '\n');
});
"#;

    async fn release_test_harness(statuses: Value) -> (tempfile::TempDir, Arc<CodexHarness>) {
        let directory = tempfile::tempdir().unwrap();
        let service = CodexHarness::new(directory.path()).unwrap();
        let mut command = tokio::process::Command::new("node");
        command
            .args(["-e", RELEASE_TEST_ENGINE, &statuses.to_string()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_create_hidden_process());
        service
            .install_test_process(
                crate::codex_service::RunningService::for_test(command.spawn().unwrap()).unwrap(),
            )
            .await;
        service.prepare().await.unwrap();
        (directory, service)
    }

    #[tokio::test]
    async fn release_ignores_projection_entries_without_native_execution() {
        let (_directory, service) = release_test_harness(json!({
            "opened":{"type":"idle"},
            "upstream-failed":{"type":"systemError"}
        }))
        .await;
        {
            let mut projection = service.projection.lock().await;
            // 打开会话时 markRead 注入的已读事件只补出 readState，没有原生 status；
            // 旧判定把它当成执行中，导致安装器在空闲时也返回 codex_active。
            projection.ingest(json!({"method":"thread/readState/changed","params":{
                "threadId":"opened","readState":{"firstUnread":null,"revision":"r1"}}}));
            projection.ingest(json!({"method":"thread/tokenUsage/updated","params":{
                "threadId":"never-loaded","tokenUsage":{"total":1}}}));
            assert!(projection.threads["opened"].get("status").is_none());
        }
        let blockers = service.release_blockers(Duration::from_millis(200)).await;
        assert!(blockers.is_empty(), "{blockers:?}");
        assert!(service.idle().await);
        service.release().await;
    }

    #[tokio::test]
    async fn release_blocks_native_active_unknown_and_in_flight_requests() {
        let (_directory, service) = release_test_harness(json!({
            "running":{"type":"active","activeFlags":["waitingOnApproval"]},
            "working":{"type":"active","activeFlags":[]},
            "mystery":{"type":"mystery"},
            "idle":{"type":"idle"}
        }))
        .await;
        let hanging = service.clone();
        let hang = tokio::spawn(async move {
            let _ = hanging.runtime.request("test/hang", json!({}), None).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let blockers = service.release_blockers(Duration::from_millis(200)).await;
        hang.abort();
        let kinds: Vec<_> = blockers
            .iter()
            .map(|blocker| {
                (
                    blocker["kind"].as_str().unwrap(),
                    blocker["threadId"].as_str(),
                )
            })
            .collect();
        assert!(kinds.contains(&("request_in_flight", None)), "{blockers:?}");
        assert!(
            kinds.contains(&("turn_waiting", Some("running"))),
            "{blockers:?}"
        );
        assert!(
            kinds.contains(&("turn_active", Some("working"))),
            "{blockers:?}"
        );
        assert!(
            kinds.contains(&("native_status_unknown", Some("mystery"))),
            "{blockers:?}"
        );
        assert!(
            !kinds.iter().any(|(_, id)| *id == Some("idle")),
            "{blockers:?}"
        );
        let in_flight = blockers
            .iter()
            .find(|blocker| blocker["kind"] == "request_in_flight")
            .unwrap();
        assert_eq!(in_flight["methods"], json!(["test/hang"]));
        assert!(!service.idle().await);
        service.release().await;
    }

    /// 后台终端：exec_command 让出后轮次结束、会话回到 idle，命令仍在运行，必须按执行中列出，
    /// 直到同一连接收到该条目的 item/completed；旧连接代次的事件不能改动当前记录。
    #[tokio::test]
    async fn background_terminals_are_tracked_until_their_command_completes() {
        let (_directory, service) = release_test_harness(json!({"idle":{"type":"idle"}})).await;
        let generation = loop {
            let current = service.connection_identity.lock().await.0.clone();
            if !current.is_empty() {
                break current;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let event = |method: &str, generation: &str, status: &str| {
            json!({"method":method,"_generation":generation,"params":{"threadId":"idle","turnId":"t",
                "item":{"type":"commandExecution","id":"call-bg","status":status,
                    "command":"npm run dev","processId":"61802","source":"unifiedExecStartup"}}})
        };
        service
            .observe_open_commands(&event("item/started", &generation, "inProgress"))
            .await;
        let blockers = service.release_blockers(Duration::ZERO).await;
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert_eq!(blockers[0]["kind"], "background_terminal");
        assert_eq!(blockers[0]["threadId"], "idle");
        assert_eq!(shutdown_effect(&blockers[0]), ShutdownEffect::Execution);
        assert!(describe_release_blocker(&blockers[0]).contains("npm run dev"));
        assert!(!service.idle().await);
        service
            .observe_open_commands(&event("item/completed", "stale-generation", "completed"))
            .await;
        assert_eq!(service.release_blockers(Duration::ZERO).await.len(), 1);
        service
            .observe_open_commands(&event("item/completed", &generation, "completed"))
            .await;
        assert!(service.release_blockers(Duration::ZERO).await.is_empty());
        assert!(service.idle().await);
        service.release().await;
    }

    #[tokio::test]
    #[ignore = "只读验证已安装引擎，需显式指定会话和轮次"]
    async fn installed_engine_history_reentry_smoke() {
        let id = std::env::var("CODEX_ELVES_HISTORY_CHECK_THREAD").expect("缺少待核对会话");
        let turn_id = std::env::var("CODEX_ELVES_HISTORY_CHECK_TURN").expect("缺少待核对轮次");
        let expected = std::env::var("CODEX_ELVES_HISTORY_CHECK_TEXT").expect("缺少待核对用户消息");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../temp");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("history-reentry-check-")
            .tempdir_in(root)
            .unwrap();
        let service = CodexHarness::new(directory.path()).unwrap();
        let mut cursor = Value::Null;
        let mut seen = std::collections::HashSet::new();
        let turn = loop {
            let history = service
                .call("history", json!({"threadId":id,"cursor":cursor}), None)
                .await
                .unwrap();
            if let Some(turn) = history["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|turn| turn["id"] == turn_id)
            {
                break turn.clone();
            }
            cursor = history["nextCursor"].clone();
            assert!(
                cursor.is_string() && seen.insert(cursor.to_string()),
                "未找到指定轮次"
            );
        };
        let first = turn["items"].as_array().unwrap();
        assert!(first.iter().any(|item| {
            item["type"] == "userMessage"
                && item["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["text"] == expected))
        }));
        let mut ids: std::collections::HashSet<_> = first
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect();
        let mut cursor = turn["itemsNextCursor"].clone();
        let mut pages = 1;
        while !cursor.is_null() {
            let history = service
                .call(
                    "history",
                    json!({"threadId":id,"turnId":turn_id,
                "itemCursor":cursor,"turnStatus":turn["status"]}),
                    None,
                )
                .await
                .unwrap();
            let before = ids.len();
            ids.extend(
                history["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item["id"].as_str().unwrap().to_owned()),
            );
            assert!(ids.len() > before, "补页没有新增记录");
            assert_ne!(history["itemsNextCursor"], cursor, "消息游标没有前进");
            cursor = history["itemsNextCursor"].clone();
            pages += 1;
        }
        assert!(
            service.loaded.lock().await.is_empty(),
            "只读核对不能接管或恢复用户会话"
        );
        println!(
            "HISTORY_REENTRY_VERIFIED {}",
            json!({"initialItems":first.len(),
            "uniqueItemsAfterPaging":ids.len(),"pages":pages,"inputVisibleOnReentry":true,
            "ownedThreads":0})
        );
        service.release().await;
    }

    #[tokio::test]
    #[ignore]
    async fn installed_engine_prewarm_smoke() {
        let directory = tempfile::tempdir().unwrap();
        let service = CodexHarness::new(directory.path()).unwrap();
        let started = std::time::Instant::now();
        service.prewarm().await.unwrap();
        service
            .catalog
            .find(&service.runtime, &service.projection, |_| false)
            .await
            .unwrap();
        let preparation_ms = started.elapsed().as_secs_f64() * 1000.0;
        let first_use = std::time::Instant::now();
        let sidebar = service.call("sidebar", json!({}), None).await.unwrap();
        println!(
            "HARNESS_PREWARM_READY {}",
            json!({
                "preparationMs":preparation_ms,"firstUseMs":first_use.elapsed().as_secs_f64()*1000.0,
                "complete":sidebar["complete"],"rows":sidebar["recent"].as_array().unwrap().len(),
                "dispatchEnabled":service.enabled.load(Ordering::Acquire),
                "ownedThreads":service.loaded.lock().await.len()
            })
        );
        assert_eq!(sidebar["complete"], true);
        assert!(!service.enabled.load(Ordering::Acquire));
        assert!(service.loaded.lock().await.is_empty());
        service.stop_preparation().await;
    }

    #[tokio::test]
    #[ignore]
    async fn installed_engine_performance_smoke() {
        let directory = tempfile::tempdir().unwrap();
        let service = CodexHarness::new(directory.path()).unwrap();
        let started = std::time::Instant::now();
        let first = service.call("sidebar", json!({}), None).await.unwrap();
        let first_ms = started.elapsed().as_secs_f64() * 1000.0;
        let warm_started = std::time::Instant::now();
        let warm = service.call("sidebar", json!({}), None).await.unwrap();
        let warm_ms = warm_started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(first["recent"], warm["recent"]);
        println!(
            "HARNESS_CATALOG_FIRST {}",
            json!({
                "firstMs":first_ms,"firstRows":first["recent"].as_array().unwrap().len(),
                "firstComplete":first["complete"],"warmMs":warm_ms
            })
        );
        let model_started = std::time::Instant::now();
        service.call("models", json!({}), None).await.unwrap();
        let model_ms = model_started.elapsed().as_secs_f64() * 1000.0;
        let model_warm_started = std::time::Instant::now();
        service.call("models", json!({}), None).await.unwrap();
        println!(
            "HARNESS_MODEL_READ {}",
            json!({
                "firstMs":model_ms,"warmMs":model_warm_started.elapsed().as_secs_f64()*1000.0
            })
        );
        if let Some(project) = first["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|project| project["synthetic"] != true)
        {
            let context_started = std::time::Instant::now();
            service
                .workspace_context(&json!({"projectId":project["id"]}))
                .await
                .unwrap();
            println!(
                "HARNESS_PROJECT_CONTEXT {}",
                json!({
                    "milliseconds":context_started.elapsed().as_secs_f64()*1000.0
                })
            );
        }
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let sidebar = service.call("sidebar", json!({}), None).await.unwrap();
                if sidebar["complete"] == true {
                    println!(
                        "HARNESS_CATALOG_COMPLETE {}",
                        json!({
                            "totalMs":started.elapsed().as_secs_f64()*1000.0,
                            "rows":sidebar["recent"].as_array().unwrap().len()
                        })
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        service.release().await;
    }

    #[tokio::test]
    #[ignore]
    async fn installed_engine_native_pins_read_only_smoke() {
        let directory = tempfile::tempdir().unwrap();
        let service = CodexHarness::new(directory.path()).unwrap();
        service.prepare().await.unwrap();
        let expected = service
            .list_all(
                "thread/list",
                json!({
                    "limit":100,"archived":false,"sectionId":"01984de2-8f74-7c91-a3b2-5c5e937cf318",
                    "sortKey":"section_position","useStateDbOnly":true
                }),
            )
            .await
            .unwrap();
        let sidebar = service.call("sidebar", json!({}), None).await.unwrap();
        let ids = |rows: &[Value]| rows.iter().map(|row| row["id"].clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(sidebar["pinnedThreads"].as_array().unwrap()),
            ids(&expected)
        );
        println!(
            "HARNESS_NATIVE_PINS {}",
            json!({
                "nativePinnedCount":expected.len(),
                "displayedPinnedCount":sidebar["pinnedThreads"].as_array().unwrap().len(),
                "readOnly":true
            })
        );
        service.release().await;
    }

    #[tokio::test]
    #[ignore]
    async fn installed_engine_read_only_smoke() {
        let directory = tempfile::tempdir().unwrap();
        let service = CodexHarness::new(directory.path()).unwrap();
        service.prepare().await.unwrap();
        let threads = service
            .runtime
            .request("thread/list", json!({"limit":1,"archived":false}), None)
            .await
            .unwrap();
        let projects = service
            .list_all("project/list", json!({"limit":100}))
            .await
            .unwrap();
        assert!(threads["data"].is_array());
        let sidebar = service.sidebar().await.unwrap();
        let displayed_projects = sidebar["projects"].as_array().unwrap();
        let project_ids = projects
            .iter()
            .map(|project| &project["id"])
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(displayed_projects.len(), project_ids.len());
        assert!(
            displayed_projects
                .iter()
                .all(|project| project_ids.contains(&project["serverProjectId"]))
        );
        assert!(
            displayed_projects
                .iter()
                .all(|project| project["synthetic"] != true)
        );
        assert_eq!(
            displayed_projects
                .iter()
                .map(|project| &project["id"])
                .collect::<Vec<_>>(),
            projects
                .iter()
                .map(|project| &project["id"])
                .collect::<Vec<_>>(),
            "独立服务的项目顺序应来自原生 project/list"
        );
        let assigned = sidebar["recent"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|thread| thread["projectId"].is_string())
            .count();
        println!(
            "HARNESS_SAVED_PROJECTS {}",
            json!({
                "savedProjects":projects.len(),"displayedProjects":displayed_projects.len(),
                "firstPageThreads":sidebar["recent"].as_array().unwrap().len(),
                "assignedThreads":assigned,
                "projectlessThreads":sidebar["recent"].as_array().unwrap().iter()
                    .filter(|thread|thread["projectId"].is_null()).count()
            })
        );
        if let Some(id) = threads["data"]
            .as_array()
            .and_then(|rows| rows.first())
            .and_then(|row| row["id"].as_str())
        {
            let goal = service
                .runtime
                .request("thread/goal/get", json!({"threadId":id}), None)
                .await;
            assert!(goal.is_ok(), "goal read failed: {goal:?}");
            let turns = service
                .runtime
                .request(
                    "thread/turns/list",
                    json!({"threadId":id,"limit":1,"itemsView":"full","sortDirection":"desc"}),
                    None,
                )
                .await
                .unwrap();
            assert!(turns["data"].is_array());
        }
        service.release().await;
    }

    #[tokio::test]
    async fn queued_ephemeral_side_message_triggers_conversation_naming_after_delivery() {
        let (directory, authorization) = super::super::tests::harness_fixture().await;
        let controller = &authorization.controller;
        let parent = controller
            .call_authorized(
                "create",
                json!({"projectId":"project-a","prompt":"wait 侧边父会话"}),
                &authorization,
            )
            .await
            .unwrap();
        let side = controller
            .call_authorized(
                "createSideThread",
                json!({"threadId":parent["threadId"]}),
                &authorization,
            )
            .await
            .unwrap();
        let side_id = side["threadId"].as_str().unwrap().to_owned();
        crate::settings::atomic_write(
            &directory.path().join("remote-shared-settings.json"),
            &serde_json::to_vec(&json!({
                "revision":1,
                "values":{"conversationNaming":{
                    "enabled":true,"instanceId":"codex:local","model":"fixture-model",
                    "systemPrompt":crate::conversation_naming::DEFAULT_PROMPT,
                    "maxTitleChars":36,"refreshEnabled":true,"refreshEveryTurns":5
                }}
            }))
            .unwrap(),
        )
        .unwrap();

        {
            let mut projection = controller.harness.projection.lock().await;
            projection
                .active
                .insert(side_id.clone(), "blocking-turn".into());
            let side = projection.sides.threads.get_mut(&side_id).unwrap();
            side.queue.queue_gates.insert(
                side_id.clone(),
                store::QueueGate {
                    turn_id: Some("blocking-turn".into()),
                    kind: "turn".into(),
                    phase: "waiting".into(),
                    reason: String::new(),
                },
            );
        }
        let queued = controller
            .call_authorized(
                "send",
                json!({"threadId":side_id,"prompt":"排队后仍需自动命名",
                    "clientUserMessageId":"queued-side-naming"}),
                &authorization,
            )
            .await
            .unwrap();
        assert_eq!(queued["deliveryState"], "queued");

        let message = {
            let mut projection = controller.harness.projection.lock().await;
            projection.active.remove(&side_id);
            let side = projection.sides.threads.get_mut(&side_id).unwrap();
            side.queue.queue_gates.get_mut(&side_id).unwrap().phase = "ready".into();
            side.queue.queues[&side_id][0].clone()
        };
        controller
            .harness
            .dispatch_side_queued(&side_id, &message, &authorization, false)
            .await
            .unwrap();

        let naming_path = directory.path().join("remote-conversation-naming.json");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(bytes) = std::fs::read(&naming_path)
                    && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                    && value["records"][&side_id]["side"] == true
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(naming_path).unwrap()).unwrap();
        assert_eq!(saved["records"][&side_id]["side"], true);
        controller.harness.enabled.store(false, Ordering::Release);
        controller.harness.release().await;
    }
}
