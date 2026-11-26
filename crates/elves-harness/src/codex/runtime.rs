//! Codex 仅是执行引擎；连接、请求关联和事件接收由 Harness 统一管理。
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::{Mutex, mpsc, oneshot};

use super::execution;
use super::read_cache::ReadCache;
use crate::codex_service::{ServiceDispatchGuard, ServiceTransport, spawn_service};
use crate::service_transport::{WriteFrame, guarded_write};

const TIMEOUT: Duration = Duration::from_secs(45);
// 用户要求服务模式新建后能立即打开会话：30 秒覆盖创建和首轮准备，
// 仅对本连接刚创建、尚未成功读取元数据的持久会话处理短暂空文件。
// 首次失败后最多等待 3 秒、重试 5 次；100/200/400/800/1000ms 共 2.5 秒，余量留给 RPC。
const NEW_THREAD_METADATA_WINDOW: Duration = Duration::from_secs(30);
const NEW_THREAD_METADATA_RETRY_BUDGET: Duration = Duration::from_secs(3);
const NEW_THREAD_METADATA_RETRY_DELAYS_MS: [u64; 5] = [100, 200, 400, 800, 1000];
type Pending = Arc<Mutex<HashMap<u64, PendingRequest>>>;

// 关停诊断需要说明“哪个请求还没返回”，所以在途表同时记录方法名，不只保存回调。
struct PendingRequest {
    method: String,
    reply: oneshot::Sender<Value>,
}

/// 退出判定使用的连接快照：从未建立连接、连接存活、连接已断开三者含义不同。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConnectionState {
    NoConnection,
    Alive,
    Lost,
}

fn is_empty_rollout_metadata_failure(failure: &anyhow::Error) -> bool {
    failure
        .downcast_ref::<super::super::ApiError>()
        .is_some_and(|error| {
            let message = error.message.to_ascii_lowercase();
            error.code == 503
                && !error.outcome_unknown
                && message.contains("failed to read session metadata")
                && message.contains("rollout at ")
                && message.trim_end().ends_with(" is empty")
        })
}

fn thread_writer_locked(home: &Path, id: &str) -> anyhow::Result<bool> {
    // 原生 writer 使用 UUID 锁名；仅检查现有文件的 OS 锁，不创建、删除锁文件或连接持有者。
    if uuid::Uuid::parse_str(id).is_err() {
        return Ok(false);
    }
    let file =
        match std::fs::File::open(home.join("thread-writer-locks").join(format!("{id}.lock"))) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("读取 Codex 会话写入锁失败"),
        };
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error).context("检查 Codex 会话写入锁失败"),
    }
}

#[derive(Debug)]
pub(super) struct UnsupportedMethod;

impl std::fmt::Display for UnsupportedMethod {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("执行引擎不支持此方法")
    }
}

pub(super) struct Runtime {
    audit: Arc<execution::Audit>,
    health: Mutex<Option<(Instant, String, Result<(), String>)>>,
    connection: Mutex<Option<Arc<Connection>>>,
    events: mpsc::Sender<Value>,
    next_generation: AtomicU64,
    retry_after: Mutex<Option<Instant>>,
    reads: Arc<ReadCache>,
    #[cfg(test)]
    test_process: Mutex<Option<crate::codex_service::RunningService>>,
    #[cfg(test)]
    test_only: AtomicBool,
}

pub(super) struct Connection {
    pub generation: String,
    pub generation_index: u64,
    pub user_agent: OnceLock<String>,
    codex_home: Option<PathBuf>,
    transport: ServiceTransport,
    /// 私有服务身份；断线后仍保留，关停判定据此区分“私有服务状态未知”与“stdio 子进程断开”。
    server: Option<crate::codex_server::PrivateServer>,
    alive: Arc<AtomicBool>,
    stdin: mpsc::Sender<WriteFrame>,
    write_gate: Mutex<()>,
    pending: Pending,
    next_id: AtomicU64,
    new_thread_metadata: Mutex<HashMap<String, Instant>>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.get_mut().take() {
            reader.abort();
        }
    }
}

impl Runtime {
    #[cfg(test)]
    pub fn has_test_process(&self) -> bool {
        self.test_only.load(Ordering::Acquire)
    }

    pub fn new(events: mpsc::Sender<Value>, audit: Arc<execution::Audit>) -> Self {
        Self {
            audit,
            health: Mutex::new(None),
            connection: Mutex::new(None),
            events,
            next_generation: AtomicU64::new(1),
            retry_after: Mutex::new(None),
            reads: Arc::new(ReadCache::default()),
            #[cfg(test)]
            test_process: Mutex::new(None),
            #[cfg(test)]
            test_only: AtomicBool::new(false),
        }
    }

    pub async fn connection(&self) -> anyhow::Result<Arc<Connection>> {
        let mut current = self.connection.lock().await;
        if let Some(connection) = current.as_ref().filter(|c| c.alive.load(Ordering::Acquire)) {
            return Ok(connection.clone());
        }
        if current.take().is_some() {
            *self.retry_after.lock().await = Some(Instant::now() + Duration::from_secs(3));
        }
        if self
            .retry_after
            .lock()
            .await
            .is_some_and(|until| Instant::now() < until)
        {
            bail!("远程会话引擎正在恢复，请稍后重试");
        }
        #[cfg(test)]
        let supplied = self.test_process.lock().await.take();
        #[cfg(not(test))]
        let supplied: Option<crate::codex_service::RunningService> = None;
        #[cfg(test)]
        if supplied.is_none() && self.test_only.load(Ordering::Acquire) {
            bail!("隔离测试引擎已结束，禁止回退到本机真实 Codex");
        }
        let service = match supplied {
            Some(service) => service,
            // 启动链路（私有服务 ensure、proxy 握手、stdio 兜底）的 future 较大，放到堆上：否则会内联进
            // connection/request 的状态机，调试构建下逐层累加，撑爆 2 MiB 的测试线程栈。
            None => match Box::pin(spawn_service()).await {
                Ok(service) => service,
                Err(error) => {
                    *self.retry_after.lock().await = Some(Instant::now() + Duration::from_secs(30));
                    return Err(error);
                }
            },
        };
        let generation = uuid::Uuid::new_v4().to_string();
        let generation_index = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let transport = service.transport;
        let server = service.server.clone();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let reader_pending = pending.clone();
        let reader_alive = alive.clone();
        let events = self.events.clone();
        let reader_generation = generation.clone();
        self.reads.clear().await;
        let reader_reads = self.reads.clone();
        // _server 是私有服务代次：同一代次重连时，原生会话和后台终端仍在服务里运行，需要重新接上。
        self.events
            .send(
                json!({"method":"harness/connected","_generation":generation,
                "_generationIndex":generation_index,
                "_server":server.as_ref().map(|server| server.generation.clone())}),
            )
            .await
            .context("会话事件接收器已关闭")?;
        let codex_home = service.codex_home.clone();
        let reader = execution::spawn_service(self.audit.clone(), "engine-reader", async move {
            // 持续接收事件，不能像请求兜底通道那样在等待响应时丢弃通知和审批。
            let mut child = service.child;
            let mut lines = service.stdout.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(mut frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if frame.get("method").is_some() {
                    if let Some(id) = frame["params"]["threadId"]
                        .as_str()
                        .or_else(|| frame["params"]["thread"]["id"].as_str())
                    {
                        reader_reads.invalidate(id).await;
                    }
                    if frame["method"] == "skills/changed" {
                        reader_reads.invalidate("skills").await;
                    }
                    let method = frame["method"].as_str().unwrap_or("");
                    if method == "thread/section/changed" {
                        reader_reads.invalidate("thread/list").await;
                    }
                    if method.starts_with("project/") {
                        reader_reads.invalidate("project/list").await;
                    }
                    if method.starts_with("model/") || method.starts_with("config/") {
                        reader_reads.invalidate("model/list").await;
                        reader_reads.invalidate("config/read").await;
                    }
                    frame["_generation"] = json!(&reader_generation);
                    frame["_generationIndex"] = json!(generation_index);
                    if events.send(frame).await.is_err() {
                        break;
                    }
                } else if let Some(id) = frame["id"].as_u64() {
                    if let Some(pending) = reader_pending.lock().await.remove(&id) {
                        let _ = pending.reply.send(frame);
                    }
                }
            }
            reader_alive.store(false, Ordering::Release);
            reader_pending.lock().await.clear();
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = events
                .send(json!({"method":"harness/disconnected",
                "_generation":reader_generation,"_generationIndex":generation_index}))
                .await;
        });
        let connection = Arc::new(Connection {
            generation,
            generation_index,
            user_agent: OnceLock::new(),
            codex_home,
            transport,
            server,
            alive,
            stdin: service.stdin,
            write_gate: Mutex::new(()),
            pending,
            next_id: AtomicU64::new(1),
            new_thread_metadata: Mutex::new(HashMap::new()),
            reader: Mutex::new(Some(reader)),
        });
        let initialized = connection
            .request(
                "initialize",
                json!({
                    "clientInfo":{"name":"elves-harness","version":env!("CARGO_PKG_VERSION")},
                    "capabilities":{"experimentalApi":true}
                }),
                None,
            )
            .await
            .context("远程会话引擎初始化失败");
        let initialized = match initialized {
            Ok(value) => value,
            Err(error) => {
                *self.retry_after.lock().await = Some(Instant::now() + Duration::from_secs(30));
                return Err(error);
            }
        };
        if let Some(agent) = initialized["userAgent"].as_str() {
            let _ = connection.user_agent.set(agent.to_owned());
        }
        connection
            .write(json!({"method":"initialized","params":{}}), None)
            .await?;
        *current = Some(connection.clone());
        Ok(connection)
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        guard: Option<&dyn ServiceDispatchGuard>,
    ) -> anyhow::Result<Value> {
        let connection = self.connection().await?;
        execution::transport(connection.transport.execution_label(), method, guard.is_some());
        if guard.is_some()
            && let Some(id) = params["threadId"].as_str()
        {
            self.reads.invalidate(id).await;
        }
        let thread_id = params["threadId"].as_str().map(str::to_owned);
        let result = connection.request(method, params, guard).await;
        if guard.is_some()
            && let Some(id) = thread_id
        {
            self.reads.invalidate(&id).await;
        }
        result
    }

    pub async fn read(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let connection = self.connection().await?;
        execution::transport(connection.transport.execution_label(), method, false);
        let scope = params["threadId"].as_str().unwrap_or_else(|| {
            if method == "skills/list" {
                "skills"
            } else {
                method
            }
        });
        let max_age = match method {
            "project/list" => Duration::from_secs(30),
            "model/list" | "config/read" | "account/read" | "configRequirements/read" => {
                Duration::from_secs(30)
            }
            "skills/list" => Duration::from_secs(5),
            _ => Duration::from_millis(200),
        };
        let key = serde_json::to_string(&json!([connection.generation, method, params]))?;
        self.reads
            .read(scope, key, max_age, connection.read(method, params.clone()))
            .await
    }

    pub async fn request_in_generation(
        &self,
        generation: &str,
        method: &str,
        params: Value,
        guard: &dyn ServiceDispatchGuard,
    ) -> anyhow::Result<Value> {
        let connection = self.connection().await?;
        execution::transport(connection.transport.execution_label(), method, true);
        // 临时侧边会话只属于创建它的引擎，断线后不能把旧写入交给新进程或自动 resume。
        if connection.generation != generation {
            return Err(super::error(410, "侧边会话所属的服务连接已结束"));
        }
        let id = params["threadId"].as_str().unwrap_or("");
        self.reads.invalidate(id).await;
        let result = connection
            .request(method, params.clone(), Some(guard))
            .await;
        self.reads.invalidate(id).await;
        result
    }

    pub(super) async fn idle_side_request(&self, generation: &str, id: &str,
        guard: Option<&dyn ServiceDispatchGuard>) -> anyhow::Result<Option<Value>> {
        // 仅供已核对安全空闲的服务自有侧边使用；不能为了后台回收启动新引擎或复用设备写入授权。
        let connection = self.connection.lock().await.clone()
            .filter(|connection| connection.generation == generation && connection.alive.load(Ordering::Acquire));
        let Some(connection) = connection else { return Ok(None) };
        let method = if guard.is_some() { "thread/unsubscribe" } else { "thread/read" };
        execution::transport(connection.transport.execution_label(), method, guard.is_some());
        let result = connection.request(method, json!({"threadId":id,"includeTurns":false}), guard).await;
        if guard.is_some() { self.reads.invalidate(id).await; }
        result.map(Some)
    }

    pub(super) async fn idle_side_goal(&self, generation: &str, id: &str) -> anyhow::Result<Option<Value>> {
        let connection = self.connection.lock().await.clone()
            .filter(|connection| connection.generation == generation && connection.alive.load(Ordering::Acquire));
        let Some(connection) = connection else { return Ok(None) };
        connection.request("thread/goal/get", json!({"threadId":id}), None).await.map(Some)
    }

    pub(super) async fn external_thread_writer(
        &self,
        id: &str,
        owned_generation: Option<&str>,
    ) -> anyhow::Result<bool> {
        let connection = self.connection().await?;
        // 用户要求其他 Codex 持有时隐藏输入框；本服务同代次的原生 writer 不能误判为外部占用。
        if owned_generation == Some(connection.generation.as_str()) {
            return Ok(false);
        }
        let Some(home) = connection.codex_home.clone() else {
            return Ok(false);
        };
        let lock_id = id.to_owned();
        let locked = tokio::task::spawn_blocking(move || thread_writer_locked(&home, &lock_id))
            .await
            .context("检查 Codex 会话占用失败")??;
        if !locked || connection.transport != ServiceTransport::PrivateServer {
            return Ok(locked);
        }
        // 用户要求重启、升级或重连后，Elves 私有服务保留的会话不能误报为外部占用。
        // loaded 只证明本连接已应用写入配置；缺少记录时须只读核对当前私有服务的真实归属，
        // 不能补写 loaded、resume 会话或查询 Codex App，否则会跳过后续配置加载或抢占外部会话。
        let mut params = json!({"limit":100});
        let mut cursors = HashSet::new();
        loop {
            execution::transport(
                connection.transport.execution_label(),
                "thread/loaded/list",
                false,
            );
            let page = connection
                .request("thread/loaded/list", params.clone(), None)
                .await
                .context("核对 Elves 私有服务会话归属失败")?;
            anyhow::ensure!(
                connection.alive.load(Ordering::Acquire),
                "核对会话归属时 Elves 私有服务连接已断开，请重试"
            );
            let rows = page["data"]
                .as_array()
                .context("Elves 私有服务的已加载会话列表格式无效")?;
            let mut owned = false;
            for row in rows {
                let loaded_id = row
                    .as_str()
                    .or_else(|| row["id"].as_str())
                    .or_else(|| row["threadId"].as_str())
                    .filter(|id| !id.is_empty())
                    .context("Elves 私有服务返回了无效的已加载会话 ID")?;
                owned |= loaded_id == id;
            }
            if owned {
                return Ok(false);
            }
            // 只有完整查完仍未找到目标，才能把原生锁判为外部持有；失败或分页异常保留真实错误。
            let cursor = match page.get("nextCursor") {
                None | Some(Value::Null) => return Ok(true),
                Some(Value::String(cursor)) if cursor.is_empty() => return Ok(true),
                Some(Value::String(cursor)) => cursor,
                _ => bail!("Elves 私有服务的已加载会话分页游标无效"),
            };
            anyhow::ensure!(
                cursors.insert(cursor.clone()),
                "Elves 私有服务的已加载会话分页游标重复"
            );
            params["cursor"] = json!(cursor);
        }
    }

    pub async fn invalidate_read(&self, scope: &str) {
        self.reads.invalidate(scope).await;
    }

    pub async fn respond(
        &self,
        generation: &str,
        id: Value,
        result: Value,
        guard: &dyn ServiceDispatchGuard,
    ) -> anyhow::Result<()> {
        let connection = self
            .connection
            .lock()
            .await
            .clone()
            .filter(|c| c.generation == generation && c.alive.load(Ordering::Acquire))
            .context("审批所属的执行引擎已断开，请刷新会话")?;
        execution::transport(
            connection.transport.execution_label(),
            "approval-response",
            true,
        );
        connection
            .write(json!({"id":id,"result":result}), Some(guard))
            .await
    }

    pub async fn shutdown(&self) {
        let connection = self.connection.lock().await.take();
        // 用户要求断开后能独立恢复；即使旧请求仍持有连接，也须先结束旧引擎，防止两个写入者并存。
        if let Some(connection) = connection {
            connection.alive.store(false, Ordering::Release);
            connection.pending.lock().await.clear();
            if let Some(reader) = connection.reader.lock().await.take() {
                reader.abort();
                let _ = reader.await;
            }
        }
        self.reads.clear().await;
        self.health.lock().await.take();
    }

    pub async fn connection_state(&self) -> ConnectionState {
        match self.connection.lock().await.as_ref() {
            None => ConnectionState::NoConnection,
            Some(connection) if connection.alive.load(Ordering::Acquire) => ConnectionState::Alive,
            Some(_) => ConnectionState::Lost,
        }
    }

    /// 当前连接上仍在等待引擎响应的请求方法（去重、排序）；没有存活连接时为空。
    pub async fn pending_methods(&self) -> Vec<String> {
        let connection = self.connection.lock().await.clone();
        let Some(connection) = connection.filter(|c| c.alive.load(Ordering::Acquire)) else {
            return Vec::new();
        };
        let mut pending = connection.pending.lock().await;
        pending.retain(|_, request| !request.reply.is_closed());
        let mut methods: Vec<String> = pending
            .values()
            .map(|request| request.method.clone())
            .collect();
        methods.sort();
        methods.dedup();
        methods
    }

    pub async fn handoff_status(&self) -> super::HandoffStatus {
        let connection = self.connection.lock().await;
        match connection.as_ref() {
            Some(connection) if connection.alive.load(Ordering::Acquire) => {
                if connection.transport.supports_handoff() {
                    super::HandoffStatus::PrivateServer
                } else {
                    super::HandoffStatus::Stdio
                }
            }
            Some(_) => super::HandoffStatus::Unknown,
            None => super::HandoffStatus::NoConnection,
        }
    }

    /// 当前（含已断开）连接对应的私有服务；stdio 或从未连接时为空。
    pub async fn private_server(&self) -> Option<crate::codex_server::PrivateServer> {
        self.connection
            .lock()
            .await
            .as_ref()
            .and_then(|connection| connection.server.clone())
    }

    pub async fn check_health(&self) -> anyhow::Result<()> {
        let connection = self.connection().await?;
        let mut health = self.health.lock().await;
        // 用户要求服务健康不依赖桌面注入；5 秒内复用探测结果，限制每分钟最多 12 次只读 RPC，
        // 单次最多等 3 秒；不 resume 会话或发送消息，也不以普通读取缓存冒充引擎可用。
        if let Some((at, generation, result)) = &*health
            && generation == &connection.generation
            && at.elapsed() < Duration::from_secs(5)
        {
            return result.clone().map_err(anyhow::Error::msg);
        }
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            connection.request("thread/list", json!({"limit":1,"archived":false}), None),
        )
        .await
        .map_err(|_| "远程会话引擎健康检查超时".to_owned())
        .and_then(|result| result.map(|_| ()).map_err(|error| error.to_string()));
        *health = Some((
            Instant::now(),
            connection.generation.clone(),
            result.clone(),
        ));
        result.map_err(anyhow::Error::msg)
    }

    #[cfg(test)]
    pub async fn install_test_process(&self, process: crate::codex_service::RunningService) {
        self.test_only.store(true, Ordering::Release);
        *self.test_process.lock().await = Some(process);
    }
}

impl Connection {
    async fn read(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        if method != "thread/read" {
            return self.request(method, params, None).await;
        }
        let thread_id = params["threadId"].as_str().unwrap_or("");
        let created_at = self
            .new_thread_metadata
            .lock()
            .await
            .get(thread_id)
            .copied()
            .filter(|at| at.elapsed() < NEW_THREAD_METADATA_WINDOW);
        let mut result = self.request(method, params.clone(), None).await;
        if let Some(created_at) = created_at {
            let deadline = (created_at + NEW_THREAD_METADATA_WINDOW)
                .min(Instant::now() + NEW_THREAD_METADATA_RETRY_BUDGET);
            for delay_ms in NEW_THREAD_METADATA_RETRY_DELAYS_MS {
                let Err(failure) = &result else { break };
                let delay = Duration::from_millis(delay_ms);
                if !is_empty_rollout_metadata_failure(failure) || Instant::now() + delay >= deadline
                {
                    break;
                }
                // 只重复同一连接的只读请求；创建、发送、恢复和结果未知的写入均不得重放。
                tokio::time::sleep(delay).await;
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let Ok(next) =
                    tokio::time::timeout(remaining, self.request(method, params.clone(), None))
                        .await
                else {
                    break;
                };
                result = next;
            }
        }
        if result.is_ok() {
            self.new_thread_metadata.lock().await.remove(thread_id);
        }
        result
    }

    async fn write(
        &self,
        frame: Value,
        guard: Option<&dyn ServiceDispatchGuard>,
    ) -> anyhow::Result<()> {
        let _gate = self.write_gate.lock().await;
        if !self.alive.load(Ordering::Acquire) {
            bail!("远程会话引擎已断开");
        }
        let mut bytes = serde_json::to_vec(&frame)?;
        bytes.push(b'\n');
        guarded_write(&self.stdin, bytes, guard, TIMEOUT).await
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        guard: Option<&dyn ServiceDispatchGuard>,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.retain(|_, request| !request.reply.is_closed());
            anyhow::ensure!(pending.len() < 128, "远程会话请求过多，请稍后再试");
            pending.insert(
                id,
                PendingRequest {
                    method: method.to_owned(),
                    reply: sender,
                },
            );
        }
        if let Err(error) = self
            .write(json!({"id":id,"method":method,"params":params}), guard)
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        let response = tokio::time::timeout(TIMEOUT, receiver).await;
        self.pending.lock().await.remove(&id);
        let frame = response
            .context("请求已派发，等待执行引擎响应超时")?
            .context("请求已派发，执行引擎连接中断")?;
        if let Some(error) = frame.get("error") {
            let rejected = matches!(error["code"].as_i64(), Some(-32600 | -32601 | -32602));
            // 引擎内部错误可能出现在写入之后，不能当作“尚未发送”而允许重放。
            let failure = anyhow::Error::from(super::super::ApiError {
                code: if rejected { 409 } else { 503 },
                message: format!(
                    "Codex 操作失败：{}",
                    error["message"].as_str().unwrap_or("未知错误")
                ),
                outcome_unknown: guard.is_some() && !rejected,
            });
            // 仅 -32601 允许兼容旧引擎；权限、参数、内部错误或超时不能伪装成不支持置顶。
            return Err(if error["code"] == -32601 {
                failure.context(UnsupportedMethod)
            } else {
                failure
            });
        }
        let result = frame.get("result").cloned().unwrap_or(Value::Null);
        if matches!(method, "thread/start" | "thread/fork")
            && result["thread"]["ephemeral"] != true
            && let Some(id) = result["thread"]["id"].as_str().filter(|id| !id.is_empty())
        {
            let mut threads = self.new_thread_metadata.lock().await;
            threads.retain(|_, at| at.elapsed() < NEW_THREAD_METADATA_WINDOW);
            threads.insert(id.to_owned(), Instant::now());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::HandoffStatus;
    use crate::codex_service::{RunningService, ServiceTransport};

    const TEST_ENGINE_SCRIPT: &str = r#"
const readline = require('readline');
const input = readline.createInterface({input: process.stdin});
let loadedPages = {'': {data: [], nextCursor: null}};
const reads = [];
input.on('line', line => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  let result;
  switch (request.method) {
    case 'initialize': result = {userAgent: 'test-engine'}; break;
    case 'test/loadedPages': loadedPages = request.params; result = {}; break;
    case 'test/reads': result = reads; break;
    case 'thread/loaded/list': {
      reads.push(request);
      const page = loadedPages[request.params.cursor || ''];
      if (!page || page.error) {
        process.stdout.write(JSON.stringify({id: request.id,
          error: page?.error || {code: -32602, message: 'unknown cursor'}}) + '\n');
        return;
      }
      result = page;
      break;
    }
    default:
      process.stdout.write(JSON.stringify({id: request.id,
        error: {code: -32601, message: 'unexpected write or read: ' + request.method}}) + '\n');
      return;
  }
  process.stdout.write(JSON.stringify({id: request.id, result}) + '\n');
});
"#;

    async fn runtime_with_transport(transport: ServiceTransport) -> Runtime {
        runtime_with_home(transport, None).await
    }

    async fn runtime_with_home(transport: ServiceTransport, codex_home: Option<PathBuf>) -> Runtime {
        let (events, _receiver) = mpsc::channel(8);
        let runtime = Runtime::new(events, Arc::new(execution::Audit::default()));
        let mut command = tokio::process::Command::new("node");
        command
            .args(["-e", TEST_ENGINE_SCRIPT])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_create_hidden_process());
        let mut process = RunningService::for_test_with_transport(
            command.spawn().expect("启动隔离测试引擎失败"),
            transport,
        )
        .expect("构造隔离测试服务失败");
        process.codex_home = codex_home;
        runtime.install_test_process(process).await;
        runtime.connection().await.expect("隔离测试引擎初始化失败");
        runtime
    }

    fn hold_writer(home: &Path, id: &str) -> std::fs::File {
        let locks = home.join("thread-writer-locks");
        std::fs::create_dir_all(&locks).unwrap();
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(locks.join(format!("{id}.lock")))
            .unwrap();
        owner.lock().unwrap();
        owner
    }

    #[tokio::test]
    async fn private_writer_survives_missing_or_old_connection_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let _owner = hold_writer(directory.path(), &id);
        let runtime = runtime_with_home(
            ServiceTransport::PrivateServer,
            Some(directory.path().to_owned()),
        )
        .await;
        runtime
            .request(
                "test/loadedPages",
                json!({"":{"data":[id],"nextCursor":null}}),
                None,
            )
            .await
            .unwrap();
        for ownership in [None, Some("previous-connection")] {
            assert!(
                !runtime.external_thread_writer(&id, ownership).await.unwrap(),
                "重启或重连后，私有服务仍持有的会话不能变成外部占用"
            );
        }
        // 已加载列表包含空闲和执行中会话；核对归属无需恢复会话或提前套用写入配置。
        let reads = runtime.request("test/reads", json!({}), None).await.unwrap();
        assert_eq!(reads.as_array().unwrap().len(), 2);
        assert!(thread_writer_locked(directory.path(), &id).unwrap());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn private_writer_checks_all_pages_and_rechecks_released_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let owner = hold_writer(directory.path(), &id);
        let runtime = runtime_with_home(
            ServiceTransport::PrivateServer,
            Some(directory.path().to_owned()),
        )
        .await;
        let first_page: Vec<_> = (0..100)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        runtime
            .request(
                "test/loadedPages",
                json!({
                    "":{"data":first_page,"nextCursor":"page-2"},
                    "page-2":{"data":[{"threadId":id}],"nextCursor":null}
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!runtime.external_thread_writer(&id, None).await.unwrap());
        let reads = runtime.request("test/reads", json!({}), None).await.unwrap();
        assert_eq!(reads.as_array().unwrap().len(), 2);
        assert_eq!(reads[1]["params"]["cursor"], "page-2");

        // 原生服务已卸载而锁转交给别的应用时，不得沿用上一次归属结果放行。
        runtime
            .request(
                "test/loadedPages",
                json!({"":{"data":[],"nextCursor":null}}),
                None,
            )
            .await
            .unwrap();
        assert!(runtime.external_thread_writer(&id, None).await.unwrap());
        owner.unlock().unwrap();
        assert!(!runtime.external_thread_writer(&id, None).await.unwrap());
        let reads = runtime.request("test/reads", json!({}), None).await.unwrap();
        assert_eq!(reads.as_array().unwrap().len(), 3, "无锁时无需查询原生归属");
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn private_writer_rejects_incomplete_or_failed_ownership_checks() {
        let directory = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let _owner = hold_writer(directory.path(), &id);
        let runtime = runtime_with_home(
            ServiceTransport::PrivateServer,
            Some(directory.path().to_owned()),
        )
        .await;
        for pages in [
            json!({"":{"error":{"code":-32000,"message":"native inspection failed"}}}),
            json!({"":{"data":null,"nextCursor":null}}),
            json!({"":{"data":[{}],"nextCursor":null}}),
            json!({"":{"data":[],"nextCursor":42}}),
            json!({"":{"data":[],"nextCursor":"loop"},"loop":{"data":[],"nextCursor":"loop"}}),
        ] {
            runtime
                .request("test/loadedPages", pages.clone(), None)
                .await
                .unwrap();
            assert!(
                runtime.external_thread_writer(&id, None).await.is_err(),
                "归属查询不完整时不能假定锁属于外部或本服务：{pages}"
            );
        }
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn handoff_status_distinguishes_stdio_and_private_server() {
        let (events, _receiver) = mpsc::channel(8);
        let empty = Runtime::new(events, Arc::new(execution::Audit::default()));
        assert_eq!(empty.handoff_status().await, HandoffStatus::NoConnection);

        let stdio = runtime_with_transport(ServiceTransport::Stdio).await;
        assert_eq!(stdio.handoff_status().await, HandoffStatus::Stdio);
        assert!(!stdio.handoff_status().await.preserves_execution());
        stdio.shutdown().await;

        let private = runtime_with_transport(ServiceTransport::PrivateServer).await;
        assert_eq!(private.handoff_status().await, HandoffStatus::PrivateServer);
        assert!(private.handoff_status().await.preserves_execution());
        private.shutdown().await;
    }
}
