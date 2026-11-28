//! SDK 对端：在 Rust 里直接驱动 WSL 内的 Claude CLI（stream-json 控制协议），并对上层说与旧共享对端
//! 相同的帧协议，所以上层的队列、视图和历史投影不需要改动。
//!
//! 分工：
//! - 执行、审批、问答、计划确认、中断、模型/权限/等级切换、消息回执：来自 CLI 的结构化帧（本文件 + state.rs）。
//! - 历史正文、网页消息身份与待确认列表、/goal 与 /compact 这类需要原生记录佐证的状态：由 WSL 内的
//!   读取器（claude_shared.py）按原生 JSONL 规范化，对端只合并实时字段后转交上层。
//! - CLI 由 WSL 内的宿主（host.py）持有，Harness 退出或安装期间执行中的任务继续；本文件只经宿主的套接字通道说话。
//!
//! 一个对端对应一个网页会话；CLI 按需启动（首次发送、压缩、权限设置），空闲一段时间后优雅退出，
//! 下次发送用 `--resume` 接上同一个原生会话。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};

use super::state::{AnswerError, Observed, PeerState, Phase, Signal};

/// 单个帧的大小上限，与上层 `MAX_SHARED_FRAME_BYTES` 一致：共享快照含整段原生历史投影。
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const CLI_CONTROL_TIMEOUT: Duration = Duration::from_secs(60);
const READER_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// 写入 CLI 后等待 `command_lifecycle: queued` 回执的时间；回执通常在几毫秒内到达。
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// 等待宿主的第一帧（host_hello 或 host_exit）；新启动时包含宿主和 CLI 自身的启动时间。
const ATTACH_TIMEOUT: Duration = Duration::from_secs(25);
const PREVIEW_INTERVAL: Duration = Duration::from_millis(120);
const SNAPSHOT_COALESCE: Duration = Duration::from_millis(25);
/// 用户要求缩短首条消息的等待：网页输入框开始输入时提前启动 CLI（`shared_warm`）。预热的 CLI 只占用资源、
/// 还没有真实使用，所以空闲 2 分钟没有发送消息就收回；一旦写入过用户消息，恢复正常的空闲回收时间。
const WARM_IDLE_STOP: Duration = Duration::from_secs(120);
/// 用户要求缩短操作等待：下游（解析、投影、合并）处理一份共享快照的耗时与体量近似成正比
/// （实测含 19 MB 历史的会话约 1 秒，并持有会话状态锁的一部分时间）。大会话的相邻两份快照至少隔开
/// `体量 × 每字节纳秒数`，期间的变化合并为一份最新的；小会话的间隔小于合并等待，不受影响。
/// 需要立即可见的回执走 `emit_now`，不受此限制。
const SNAPSHOT_NANOS_PER_BYTE: u64 = 50;
const SNAPSHOT_MAX_SPACING: Duration = Duration::from_secs(3);

fn snapshot_spacing(bytes: usize) -> Duration {
    Duration::from_nanos((bytes as u64).saturating_mul(SNAPSHOT_NANOS_PER_BYTE))
        .min(SNAPSHOT_MAX_SPACING)
}
/// 宿主收尾最长 35 秒（host.py：STOP_TIMEOUT 30 秒后 SIGTERM，再等 KILL_GRACE 5 秒后强制结束），这里多留余量。
const GRACEFUL_EXIT: Duration = Duration::from_secs(40);
/// 停止请求发出后最多等多久：CLI 通常几毫秒内回执并转为空闲；超过它既没有回执、会话也没有空闲，
/// 就按“CLI 没有响应”处理，强制结束会话进程，让停止有确定的结果。
const INTERRUPT_WAIT: Duration = Duration::from_secs(8);
/// 强制结束 CLI 后等它退出的上限：宿主先 SIGTERM，再过 KILL_GRACE（5 秒）补 SIGKILL，这里多留余量。
const FORCED_EXIT: Duration = Duration::from_secs(15);

pub(in crate::claude) struct Io {
    pub(in crate::claude) writer: Box<dyn AsyncWrite + Send + Unpin>,
    pub(in crate::claude) reader: Box<dyn AsyncRead + Send + Unpin>,
    pub(in crate::claude) stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    /// 持有真实子进程；丢弃时由 kill_on_drop 回收。
    pub(in crate::claude) guard: Box<dyn Send>,
}

/// 每次连接 CLI 所需的会话参数；设置接口会更新它们，下次启动（含空闲回收后的重新启动）按最新值启动。
#[derive(Clone, Debug, Default)]
pub(in crate::claude) struct CliSpec {
    pub(in crate::claude) native_id: String,
    pub(in crate::claude) resumed: bool,
    /// CLI 认可的权限模式名；`None` 沿用 CLI 自己的配置。
    pub(in crate::claude) permission_mode: Option<String>,
    /// 只接入已经在运行的宿主，不存在就不启动新的 CLI（重启后恢复执行中的任务时使用）。
    pub(in crate::claude) attach_only: bool,
}

#[async_trait]
pub(in crate::claude) trait Backend: Send + Sync + 'static {
    async fn spawn_cli(&self, spec: &CliSpec) -> anyhow::Result<Io>;
    async fn spawn_reader(&self, native_id: &str) -> anyhow::Result<Io>;
}

#[derive(Clone, Debug)]
pub(in crate::claude) struct PeerSpec {
    pub(in crate::claude) native_id: String,
    pub(in crate::claude) resumed: bool,
    pub(in crate::claude) idle_stop: Duration,
}

#[derive(Debug)]
struct PeerError {
    code: Option<&'static str>,
    message: String,
    /// 明确没有写入 CLI：上层据此决定可以安全重试，还是只能核对。
    not_sent: bool,
}

impl PeerError {
    fn new(code: &'static str, message: impl Into<String>, not_sent: bool) -> Self {
        Self {
            code: Some(code),
            message: message.into(),
            not_sent,
        }
    }
    fn plain(message: impl Into<String>, not_sent: bool) -> Self {
        Self {
            code: None,
            message: message.into(),
            not_sent,
        }
    }
}

type Reply = Result<Value, PeerError>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Link {
    Connecting,
    Attached,
    Gone,
}

struct Cli {
    generation: u64,
    writer: mpsc::UnboundedSender<Line>,
    #[allow(dead_code)]
    guard: Box<dyn Send>,
}

struct Line {
    bytes: Vec<u8>,
    done: Option<oneshot::Sender<Result<(), String>>>,
}

struct ReaderConn {
    writer: mpsc::UnboundedSender<Line>,
    #[allow(dead_code)]
    guard: Box<dyn Send>,
    /// 第几次启动的读取器：旧读取器迟到的结束通知不能摘掉后来重新启动的那个。
    generation: u64,
}

type CliWaiters = StdMutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>;
type ReaderWaiters = StdMutex<HashMap<String, oneshot::Sender<Result<Value, ReaderFailure>>>>;

/// 读取器操作失败：`not_sent` 说明请求是否明确没有生效（读取器回执里的标记；本地连接问题按未送达或未知区分）。
#[derive(Debug, Clone)]
struct ReaderFailure {
    code: String,
    message: String,
    not_sent: bool,
}

impl ReaderFailure {
    fn local(message: &str, not_sent: bool) -> Self {
        Self {
            code: String::new(),
            message: message.to_owned(),
            not_sent,
        }
    }
}

struct Inner {
    backend: Arc<dyn Backend>,
    spec: PeerSpec,
    /// 下次启动 CLI 使用的参数。
    launch: StdMutex<CliSpec>,
    /// 用户在网页里选择的模型与思考深度；CLI 每次启动后都要重新应用（CLI 默认值会覆盖会话上次的选择）。
    selection: StdMutex<(String, String)>,
    state: StdMutex<PeerState>,
    cli: Mutex<Option<Cli>>,
    /// 停止 CLI 与启动 CLI 互斥：停止要等 CLI 真正退出才算完成，期间不能有人去启动（连上还没退净的宿主）。
    lifecycle: Mutex<()>,
    cli_generation: AtomicU64,
    cli_pending: CliWaiters,
    /// 等待 `command_lifecycle: queued` 的消息，键是写入 CLI 的用户消息 UUID。
    acks: StdMutex<HashMap<String, oneshot::Sender<()>>>,
    link: StdMutex<Link>,
    link_changed: Notify,
    reader: Mutex<Option<ReaderConn>>,
    /// 同一时间只重新启动一个读取器。
    reader_restart: Mutex<()>,
    reader_generation: AtomicU64,
    reader_pending: ReaderWaiters,
    reader_frame: StdMutex<Option<Value>>,
    /// 当前 CLI 由预热启动且还没有写入过用户消息：空闲回收按 `WARM_IDLE_STOP`。
    warm_only: AtomicBool,
    /// 最近一份读取器快照的原始字节数，用来估算下游处理成本。
    reader_bytes: AtomicUsize,
    reader_rev: AtomicU64,
    state_rev: AtomicU64,
    emitted: StdMutex<(u64, u64)>,
    initialize_request: StdMutex<Value>,
    /// 对端自己登记的设置变更；目前只有需要重启才生效的完全访问权限。
    changes: StdMutex<Value>,
    restarting: AtomicBool,
    dirty: Notify,
    preview_dirty: Notify,
    last_preview: StdMutex<String>,
    out: mpsc::UnboundedSender<Value>,
    closed: AtomicBool,
    activity: StdMutex<tokio::time::Instant>,
    exited: Notify,
    /// 对端状态有变化（等会话空闲等场合使用）；与只唤醒发送循环的 `dirty` 分开，可有多个等待者。
    changed: Notify,
    exit_detail: StdMutex<String>,
}

/// 失败点的诊断记录：只含事件名、原生会话 ID 前缀和简短原因，不含消息正文、配置内容和 CLI 的 stderr。
/// 真实 WSL 联调时曾因为宿主丢帧白等 60 秒而没有任何线索，所以对端失败点都留一条记录。
fn diagnose(event: &str, native_id: &str, detail: Value) {
    super::super::diagnose_event(
        &format!("claude.sdk.{event}"),
        json!({"native_id": native_id.chars().take(8).collect::<String>(), "detail": detail}),
    );
}

fn now_ms() -> u64 {
    crate::now_ms()
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    // 持锁代码不会 panic；即便中毒也继续使用内部数据，避免一次异常拖垮整个对端。
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 启动对端；`host_reader`/`host_writer` 是与上层相连的一端。返回的任务在上层释放会话时结束。
pub(super) fn start(
    backend: Arc<dyn Backend>,
    spec: PeerSpec,
    host_reader: impl AsyncRead + Send + Unpin + 'static,
    host_writer: impl AsyncWrite + Send + Unpin + 'static,
) -> tokio::task::JoinHandle<()> {
    let (out, receiver) = mpsc::unbounded_channel();
    let launch = CliSpec {
        native_id: spec.native_id.clone(),
        resumed: spec.resumed,
        ..CliSpec::default()
    };
    let inner = Arc::new(Inner {
        backend,
        launch: StdMutex::new(launch),
        selection: StdMutex::new((String::new(), String::new())),
        state: StdMutex::new(PeerState::new(Vec::new())),
        spec,
        cli: Mutex::new(None),
        lifecycle: Mutex::new(()),
        cli_generation: AtomicU64::new(0),
        cli_pending: StdMutex::new(HashMap::new()),
        acks: StdMutex::new(HashMap::new()),
        link: StdMutex::new(Link::Gone),
        link_changed: Notify::new(),
        reader: Mutex::new(None),
        reader_restart: Mutex::new(()),
        reader_generation: AtomicU64::new(0),
        reader_pending: StdMutex::new(HashMap::new()),
        reader_frame: StdMutex::new(None),
        warm_only: AtomicBool::new(false),
        reader_bytes: AtomicUsize::new(0),
        reader_rev: AtomicU64::new(0),
        state_rev: AtomicU64::new(0),
        emitted: StdMutex::new((u64::MAX, u64::MAX)),
        initialize_request: StdMutex::new(Value::Null),
        changes: StdMutex::new(json!({})),
        restarting: AtomicBool::new(false),
        dirty: Notify::new(),
        preview_dirty: Notify::new(),
        last_preview: StdMutex::new(String::new()),
        out,
        closed: AtomicBool::new(false),
        activity: StdMutex::new(tokio::time::Instant::now()),
        exited: Notify::new(),
        changed: Notify::new(),
        exit_detail: StdMutex::new(String::new()),
    });
    tokio::spawn(write_host(receiver, host_writer));
    tokio::spawn(emit_loop(inner.clone()));
    tokio::spawn(preview_loop(inner.clone()));
    tokio::spawn(idle_loop(inner.clone()));
    tokio::spawn(read_host(inner, host_reader))
}

async fn write_host(
    mut receiver: mpsc::UnboundedReceiver<Value>,
    mut writer: impl AsyncWrite + Send + Unpin + 'static,
) {
    while let Some(frame) = receiver.recv().await {
        let Ok(mut bytes) = serde_json::to_vec(&frame) else {
            continue;
        };
        bytes.push(b'\n');
        if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
}

/// 读取一行 JSON。外层 `None` 表示通道已关闭或帧超限；内层 `None` 表示这一行不是有效 JSON。
async fn read_json_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> Option<Option<Value>> {
    line.clear();
    let size = (&mut *reader)
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', line)
        .await
        .ok()?;
    if size == 0 || size > MAX_FRAME_BYTES {
        return None;
    }
    Some(serde_json::from_slice::<Value>(line).ok())
}

async fn read_host(inner: Arc<Inner>, reader: impl AsyncRead + Send + Unpin + 'static) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    while let Some(parsed) = read_json_line(&mut reader, &mut line).await {
        let Some(frame) = parsed else {
            continue;
        };
        if frame["type"] != "control_request" {
            continue;
        }
        let Some(id) = frame["request_id"].as_str().map(str::to_owned) else {
            continue;
        };
        let request = frame["request"].clone();
        let inner = inner.clone();
        tokio::spawn(async move {
            inner.touch_activity();
            let result = inner.handle(&request).await;
            let response = match result {
                Ok(value) => json!({"subtype":"success","request_id":id,"response":value}),
                Err(failure) => {
                    let mut value = json!({"subtype":"error","request_id":id,"error":failure.message,
                        "not_sent":failure.not_sent});
                    if let Some(code) = failure.code {
                        value["code"] = json!(code);
                    }
                    value
                }
            };
            let _ = inner
                .out
                .send(json!({"type":"control_response","response":response}));
        });
    }
    // 上层已释放这个会话：不强行终止 CLI。执行中的任务由宿主继续，空闲则宿主随后自行收尾。
    inner.closed.store(true, Ordering::Release);
    inner.dirty.notify_one();
    inner.preview_dirty.notify_one();
    inner.cli.lock().await.take();
    inner.reader.lock().await.take();
}

impl Inner {
    fn touch_activity(&self) {
        *lock(&self.activity) = tokio::time::Instant::now();
    }

    fn bump(&self) {
        self.state_rev.fetch_add(1, Ordering::AcqRel);
        self.dirty.notify_one();
        self.changed.notify_waiters();
    }

    /// 等到会话不再忙（CLI 自己报告空闲，或进程已退出）。
    async fn wait_idle(&self) {
        loop {
            let notified = self.changed.notified();
            if !self.state(|state| state.busy()) {
                return;
            }
            notified.await;
        }
    }

    fn state<R>(&self, change: impl FnOnce(&mut PeerState) -> R) -> R {
        change(&mut lock(&self.state))
    }

    fn alive(&self) -> bool {
        self.state(|state| state.alive())
    }

    // ------------------------------------------------------------ 宿主请求

    async fn handle(self: &Arc<Self>, request: &Value) -> Reply {
        match request["subtype"].as_str().unwrap_or("") {
            "initialize" => self.initialize(request).await,
            "shared_sync" => self.sync().await,
            "get_task_output" | "stop_task" => self.shell_task_control(request).await,
            "shared_warm" => self.warm().await,
            "shared_send" => self.send(request).await,
            "shared_native_command" => self.native_command(request).await,
            "shared_answer" => self.answer(request).await,
            "shared_shutdown" => self.shutdown().await,
            "set_model" => self.set_model(request).await,
            "set_permission_mode" => self.set_permission_mode(request).await,
            "apply_flag_settings" => self.apply_flag_settings(request).await,
            "interrupt" => self.interrupt().await,
            "shared_edit_message" => self.edit_message(request).await,
            _ => Err(PeerError::plain("此原生 Claude 控制操作尚未适配", true)),
        }
    }

    async fn shell_task_control(&self, request: &Value) -> Reply {
        let id=request["task_id"].as_str().unwrap_or_default();
        let run=request["run_id"].as_str().unwrap_or_default();
        let subtype=request["subtype"].as_str().unwrap_or_default();
        let key=uuid::Uuid::new_v4().to_string();
        let (tx,rx)=oneshot::channel();
        // 与 CLI 替换互斥：仅向持有该原生 run_id 的现有 CLI 写单任务控制，不启动或中断会话。
        let sent={
            let cli=self.cli.lock().await;
            let Some(cli)=cli.as_ref().filter(|_|self.state(|s|s.owns_shell_task(id,run)
                || subtype=="get_task_output" && s.recognizes_finished_shell(id,run))) else {
                return Err(PeerError::new("ESTALE","任务已结束或运行身份已变化",true));
            };
            if subtype=="stop_task" && !self.state(|s|s.task_stop_supported()) {
                return Err(PeerError::new("EINVAL","此 Claude CLI 尚未验证单任务停止",true));
            }
            if subtype=="stop_task" && !self.state(|s|s.mark_task_stop(id,run,true)) {
                return Ok(json!({"alreadyRequested":true}));
            }
            lock(&self.cli_pending).insert(key.clone(),tx);
            Self::write_cli(cli,&json!({"type":"control_request","request_id":key,
                "request":{"subtype":subtype,"task_id":id}}))
        };
        let result=async {
            let write=sent?;
            tokio::time::timeout(CLI_CONTROL_TIMEOUT,async {
                write.await.map_err(|_|PeerError::plain("任务控制写入回执未知",false))?
                    .map_err(|e|PeerError::plain(e,false))?;
                rx.await.map_err(|_|PeerError::plain("任务控制回执未知",false))?
                    .map_err(|e|PeerError::plain(e,false))
            }).await.map_err(|_|PeerError::plain("等待任务控制回执超时",false))?
        }.await;
        if subtype=="stop_task"{
            // 不同设备请求也不重发；只有能确认没有写入才撤销标记。标记本身不是结束回执。
            if result.as_ref().is_err_and(|e|e.not_sent){self.state(|s|s.mark_task_stop(id,run,false));}
            self.bump();
        }
        lock(&self.cli_pending).remove(&key);
        result
    }

    fn settings_projection(&self) -> Value {
        let (actual, changes) = (
            self.state(|state| state.actual.clone()),
            lock(&self.changes).clone(),
        );
        let mut settings = json!({"changes":changes});
        if !actual.model.is_empty() {
            settings["model"] = json!(actual.model);
        }
        if !actual.effort.is_empty() {
            settings["effort"] = json!(actual.effort);
        }
        if !actual.permission_mode.is_empty() {
            settings["permissionMode"] = json!(actual.permission_mode);
        }
        settings
    }

    /// 立即把合并后的快照交给上层：`shared_sync`、`shared_send` 等回执之前上层必须已经看到最新状态。
    fn emit_now(&self) {
        self.emit_snapshot(true);
    }

    fn emit_snapshot(&self, force: bool) -> bool {
        // 与读取器的小事件共用此锁直到入队；正在合成的旧状态不能在“开始”之后倒发。
        let reader = lock(&self.reader_frame);
        let Some(reader) = reader.as_ref() else { return false };
        let signature = (
            self.reader_rev.load(Ordering::Acquire),
            self.state_rev.load(Ordering::Acquire),
        );
        if !force && *lock(&self.emitted) == signature { return false; }
        let changes = lock(&self.changes).clone();
        let frame = self.state(|state| state.compose(reader, &changes));
        *lock(&self.emitted) = signature;
        self.out.send(frame).is_ok()
    }

    /// 读取器和实时状态都没有变化时，上层手里的就是最新状态，不再复制并重发整份历史。
    /// 用户要求缩短发送前的等待：`shared_sync` 回执前上层只需看到最新状态，不要求再收一份相同的快照；
    /// 大会话每多发一份快照都要重新序列化、传输和解析全部历史（实测 8290 条消息、21.6 MiB 的会话
    /// 仅读取器侧就要 0.17–0.18 秒）。有变化时照常立即发出。
    fn emit_if_changed(&self) {
        self.emit_snapshot(false);
    }

    /// 提前启动 CLI。只有 CLI 原本没有运行时才算预热；已经在运行的 CLI 保持原来的空闲回收时间。
    async fn warm(self: &Arc<Self>) -> Reply {
        // 读取器已退出时（例如重启过 WSL），开始输入就重新启动它，网页内容随即补齐，不用等到发送。
        self.ensure_reader().await.map_err(reader_error)?;
        let was_alive = self.alive();
        self.ensure_cli(false).await?;
        if !was_alive {
            self.warm_only.store(true, Ordering::Release);
        }
        self.touch_activity();
        Ok(json!({"warm":!was_alive}))
    }

    async fn sync(self: &Arc<Self>) -> Reply {
        self.reader_control(json!({"subtype":"shared_sync"}))
            .await
            .map_err(reader_error)?;
        self.emit_if_changed();
        Ok(json!({"nativeSettings":self.settings_projection()}))
    }

    async fn initialize(self: &Arc<Self>, request: &Value) -> Reply {
        *lock(&self.initialize_request) = request.clone();
        let modes: Vec<String> = request["catalog"]["permissionModes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        *lock(&self.selection) = (
            request["model"].as_str().unwrap_or("").to_owned(),
            request["effort"].as_str().unwrap_or("").to_owned(),
        );
        lock(&self.launch).permission_mode = request["permission_mode"]
            .as_str()
            .filter(|mode| *mode != "configured")
            .map(normalize_mode);
        self.state(|state| *state = PeerState::new(modes));
        let context = self.start_reader().await?;
        if request["attach"] == true {
            // 重启前仍在执行的会话：接回已经在 WSL 内继续运行的宿主；没有就保持空闲，不启动新的 CLI。
            let _ = self.ensure_cli(true).await;
        }
        let mut catalog = request["catalog"].clone();
        catalog["contextConfiguration"] = context;
        Ok(catalog)
    }

    // ------------------------------------------------------------ CLI 连接

    /// 确保与 CLI 的通道可用；返回是否已连上 CLI。`attach_only` 时只接入已有宿主。
    async fn ensure_cli(self: &Arc<Self>, attach_only: bool) -> Result<bool, PeerError> {
        // 正在停止的 CLI 还没退净时不能去连它的宿主；等停止完成，启动期间也不让空闲回收等停止插进来。
        let _lifecycle = self.lifecycle.lock().await;
        let mut guard = self.cli.lock().await;
        if let Some(cli) = guard.as_ref()
            && self.alive()
            && !cli.writer.is_closed()
        {
            return Ok(true);
        }
        guard.take();
        let mut spec = lock(&self.launch).clone();
        spec.attach_only = attach_only;
        // 用户要求继续优化冷启动：记录每次真实启动各阶段的耗时（只一行日志），下次用真实日志定位等待在哪一段。
        let started = Instant::now();
        let io = self.backend.spawn_cli(&spec).await.map_err(|failure| {
            PeerError::plain(format!("无法启动指定 WSL 中的 Claude CLI：{failure}"), true)
        })?;
        let spawned = started.elapsed();
        let generation = self.cli_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (writer, receiver) = mpsc::unbounded_channel();
        tokio::spawn(write_lines(receiver, io.writer));
        *guard = Some(Cli {
            generation,
            writer,
            guard: io.guard,
        });
        lock(&self.exit_detail).clear();
        *lock(&self.link) = Link::Connecting;
        if let Some(stderr) = io.stderr {
            tokio::spawn(drain_stderr(stderr));
        }
        tokio::spawn(read_cli(self.clone(), generation, io.reader));
        if !attach_only {
            self.state(|state| state.set_alive(true, now_ms()));
            self.bump();
        }
        drop(guard);
        // 先等到宿主的第一帧，再写任何请求：host_hello 说明接上了（或启动了）CLI；host_exit、host_error 或通道关闭说明没有。
        // 宿主连接端在 host_hello 之前收到的内容不保证转交给 CLI（真实 WSL 联调时 initialize 因此丢过一次，白等 60 秒）。
        let attached = tokio::time::timeout(ATTACH_TIMEOUT, async {
            loop {
                let notified = self.link_changed.notified();
                let link = *lock(&self.link);
                match link {
                    Link::Connecting => notified.await,
                    Link::Attached => return true,
                    Link::Gone => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        let attached_at = started.elapsed();
        if !attached {
            if attach_only {
                self.drop_cli(generation).await;
                return Ok(false);
            }
            let reason = self.explain_exit("Claude CLI 未能启动：宿主没有响应".into());
            diagnose(
                "host_unavailable",
                &self.spec.native_id,
                json!({"reason":reason}),
            );
            self.on_cli_exit(generation).await;
            return Err(PeerError::plain(reason, true));
        }
        // 重新连接已有 CLI 时 initialize 也是安全的（实测第二次 initialize 返回同样的目录）。
        self.cli_control(
            "initialize",
            json!({"supportedDialogKinds":[],"promptSuggestions":false}),
            CLI_CONTROL_TIMEOUT,
        )
        .await
        .map_err(|failure| {
            diagnose(
                "cli_init_failed",
                &self.spec.native_id,
                json!({"error":failure}),
            );
            PeerError::plain(
                self.explain_exit(format!("Claude CLI 初始化失败：{failure}")),
                true,
            )
        })?;
        let initialized_at = started.elapsed();
        self.reapply_selection().await;
        if !attach_only {
            let ms = |duration: Duration| duration.as_millis() as u64;
            diagnose(
                "cli_started",
                &self.spec.native_id,
                json!({"spawnMs":ms(spawned),"attachMs":ms(attached_at.saturating_sub(spawned)),
                    "initializeMs":ms(initialized_at.saturating_sub(attached_at)),
                    "reapplyMs":ms(started.elapsed().saturating_sub(initialized_at)),
                    "totalMs":ms(started.elapsed()),"resumed":spec.resumed}),
            );
        }
        Ok(true)
    }

    /// CLI 重新启动后恢复用户在网页里选择的模型和思考深度。
    async fn reapply_selection(self: &Arc<Self>) {
        let (model, effort) = lock(&self.selection).clone();
        if !model.is_empty()
            && model != "default"
            && let Ok(response) = self
                .cli_control("set_model", json!({"model":model}), CLI_CONTROL_TIMEOUT)
                .await
            && let Some(actual) = response["model"].as_str()
        {
            self.state(|state| state.actual.model = actual.to_owned());
        }
        if !effort.is_empty()
            && self
                .cli_control(
                    "apply_flag_settings",
                    json!({"settings":{"effortLevel":effort}}),
                    CLI_CONTROL_TIMEOUT,
                )
                .await
                .is_ok()
        {
            self.state(|state| state.actual.effort = effort.clone());
        }
        self.bump();
    }

    fn explain_exit(&self, fallback: String) -> String {
        let detail = lock(&self.exit_detail).clone();
        if detail.is_empty() { fallback } else { detail }
    }

    async fn drop_cli(self: &Arc<Self>, generation: u64) {
        let mut guard = self.cli.lock().await;
        if guard
            .as_ref()
            .is_some_and(|cli| cli.generation == generation)
        {
            guard.take();
        }
    }

    fn write_cli(
        cli: &Cli,
        value: &Value,
    ) -> Result<oneshot::Receiver<Result<(), String>>, PeerError> {
        let mut bytes = serde_json::to_vec(value)
            .map_err(|failure| PeerError::plain(format!("Claude 请求无法编码：{failure}"), true))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(PeerError::new(
                "EINVAL",
                "Claude 输入超过大小限制，消息尚未派发",
                true,
            ));
        }
        bytes.push(b'\n');
        let (done, result) = oneshot::channel();
        cli.writer
            .send(Line {
                bytes,
                done: Some(done),
            })
            .map_err(|_| PeerError::plain("Claude 进程已退出，消息尚未派发", true))?;
        Ok(result)
    }

    async fn send_cli(&self, value: &Value) -> Result<(), PeerError> {
        let receiver = {
            let guard = self.cli.lock().await;
            let cli = guard
                .as_ref()
                .ok_or_else(|| PeerError::plain("Claude 进程已退出，消息尚未派发", true))?;
            Self::write_cli(cli, value)?
        };
        match tokio::time::timeout(Duration::from_secs(15), receiver).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(message))) => Err(PeerError::plain(
                format!("写入 Claude 失败：{message}"),
                false,
            )),
            Ok(Err(_)) => Err(PeerError::plain("Claude 连接已关闭，写入结果未知", false)),
            Err(_) => Err(PeerError::plain("写入 Claude 超时，结果未知", false)),
        }
    }

    /// 把控制请求写给 CLI 并登记等待回执；返回请求标识与回执通道。写入失败时没有登记任何东西。
    /// 调用者用完必须从 `cli_pending` 里移除这个标识（`cli_control` 已代劳）。
    async fn begin_cli_control(
        &self,
        subtype: &str,
        mut fields: Value,
    ) -> Result<(String, oneshot::Receiver<Result<Value, String>>), PeerError> {
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        lock(&self.cli_pending).insert(id.clone(), sender);
        fields["subtype"] = json!(subtype);
        let frame = json!({"type":"control_request","request_id":id,"request":fields});
        match self.send_cli(&frame).await {
            Ok(()) => Ok((id, receiver)),
            Err(failure) => {
                lock(&self.cli_pending).remove(&id);
                Err(failure)
            }
        }
    }

    /// 向 CLI 发控制请求并等待它的回执；返回回执里的 `response`。
    async fn cli_control(
        &self,
        subtype: &str,
        fields: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let (id, receiver) = self
            .begin_cli_control(subtype, fields)
            .await
            .map_err(|failure| failure.message)?;
        let outcome = match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Claude 控制请求已派发，连接已关闭".to_owned()),
            Err(_) => Err("Claude 控制请求已派发，回执等待超时".to_owned()),
        };
        lock(&self.cli_pending).remove(&id);
        outcome
    }

    async fn on_cli_frame(self: &Arc<Self>, generation: u64, frame: Value) {
        self.touch_activity();
        match frame["type"].as_str() {
            Some("host_hello") => {
                *lock(&self.link) = Link::Attached;
                self.link_changed.notify_waiters();
                self.state(|state| state.set_alive(true, now_ms()));
                self.bump();
                return;
            }
            Some("host_stderr") => return,
            Some("host_error") => {
                let message = frame["message"].as_str().unwrap_or("Claude 宿主拒绝了连接");
                *lock(&self.exit_detail) = message.to_owned();
                diagnose(
                    "host_error",
                    &self.spec.native_id,
                    json!({"message":message}),
                );
                self.on_cli_exit(generation).await;
                return;
            }
            Some("host_exit") => {
                let error = frame["error"].as_str().unwrap_or("");
                // CLI 的 stderr 可能含配置内容，不转给网页；只说明退出码或宿主报告的启动错误。
                let detail = if !error.is_empty() && error != "none" {
                    format!("Claude CLI 未能启动：{error}")
                } else if let Some(code) = frame["code"].as_i64() {
                    format!("Claude CLI 已退出（退出码 {code}），请检查所选 WSL 实例中的安装和登录")
                } else {
                    String::new()
                };
                if !detail.is_empty() {
                    diagnose(
                        "host_exit",
                        &self.spec.native_id,
                        json!({"code":frame["code"],"error":error}),
                    );
                    *lock(&self.exit_detail) = detail;
                }
                self.on_cli_exit(generation).await;
                return;
            }
            Some("control_response") => {
                let response = &frame["response"];
                let Some(id) = response["request_id"].as_str() else {
                    return;
                };
                if let Some(sender) = lock(&self.cli_pending).remove(id) {
                    let result = if response["subtype"] == "success" {
                        Ok(response["response"].clone())
                    } else {
                        Err(response["error"]
                            .as_str()
                            .unwrap_or("原生请求失败")
                            .to_owned())
                    };
                    let _ = sender.send(result);
                }
                return;
            }
            Some("command_lifecycle") => {
                if frame["state"] == "queued"
                    && let Some(uuid) = frame["command_uuid"].as_str()
                    && let Some(sender) = lock(&self.acks).remove(uuid)
                {
                    let _ = sender.send(());
                }
            }
            Some("control_request") if frame["request"]["subtype"] != "can_use_tool" => {
                self.decline_unsupported(&frame).await;
                return;
            }
            _ => {}
        }
        let now = now_ms();
        let seen = self.state(|state| state.observe_cli(&frame, now));
        if seen.dirty && frame["subtype"]=="task_notification"
            && let (Some(id),Some(run))=(frame["task_id"].as_str(),frame["run_id"].as_str())
            && self.state(|s|s.recognizes_finished_shell(id,run)) {
            // 读取任务不可阻塞 CLI 读循环（响应走同一管道）。不启动新 CLI，不读取通知中的文件路径。
            let (inner,id,run)=(self.clone(),id.to_owned(),run.to_owned());
            tokio::spawn(async move{
                let reply=inner.shell_task_control(&json!({"subtype":"get_task_output","task_id":id,"run_id":run})).await
                    .unwrap_or_else(|failure|json!({"error":failure.message}));
                if inner.state(|s|s.save_shell_output(&id,&run,&reply)){inner.bump();}
            });
        }
        self.apply(seen).await;
    }

    /// 对端没有承载 hook、MCP 回调等的能力；明确拒绝，避免 CLI 一直等待回复。
    async fn decline_unsupported(&self, frame: &Value) {
        let Some(id) = frame["request_id"].as_str() else {
            return;
        };
        let response = if frame["request"]["subtype"] == "elicitation" {
            json!({"type":"control_response","response":{"subtype":"success","request_id":id,
                "response":{"action":"cancel"}}})
        } else {
            json!({"type":"control_response","response":{"subtype":"error","request_id":id,
                "error":"Elves 的 Claude 对端不处理此类请求"}})
        };
        let _ = self.send_cli(&response).await;
    }

    async fn apply(self: &Arc<Self>, seen: Observed) {
        for signal in &seen.signals {
            let request = match signal {
                Signal::Compacting(active) => json!({"subtype":"sdk_signal","compacting":active}),
                Signal::CompactFailed(message) => {
                    json!({"subtype":"sdk_signal","compact_failed":message})
                }
            };
            // 压缩信号只影响“压缩中/失败”的显示；终态仍以原生记录里的 compact_boundary 为准。
            // 这里在读取 CLI 输出的流程里，不能为它等待读取器重新启动。
            let _ = self.reader_request(request).await;
        }
        if seen.preview {
            self.preview_dirty.notify_one();
        }
        if seen.dirty || !seen.signals.is_empty() {
            self.bump();
            self.schedule_pending_permission();
        }
    }

    async fn on_cli_exit(self: &Arc<Self>, generation: u64) {
        {
            let mut guard = self.cli.lock().await;
            if guard
                .as_ref()
                .is_some_and(|cli| cli.generation == generation)
            {
                guard.take();
            } else if guard.is_some() || self.cli_generation.load(Ordering::Acquire) != generation {
                // 已经被更新的连接取代，旧连接的收尾不能影响新连接。
                return;
            }
        }
        *lock(&self.link) = Link::Gone;
        self.link_changed.notify_waiters();
        self.state(|state| state.set_alive(false, now_ms()));
        let detail = self.explain_exit("Claude 进程已退出".to_owned());
        for (_, sender) in lock(&self.cli_pending).drain() {
            let _ = sender.send(Err(detail.clone()));
        }
        lock(&self.acks).clear();
        self.exited.notify_waiters();
        self.bump();
    }

    /// 请求宿主优雅收尾（CLI 完成手头的一轮后自行退出），并等 CLI 真正退出。
    async fn stop_cli(&self) {
        self.end_cli("host_stop", GRACEFUL_EXIT).await;
    }

    /// CLI 不响应停止请求时强制结束：宿主先 SIGTERM 让它清理，再补 SIGKILL，并等它真正退出。
    async fn kill_cli(&self) {
        self.end_cli("host_kill", FORCED_EXIT).await;
    }

    /// 向宿主发 `command`（host_stop 或 host_kill）并等 CLI 退出，最多等 `limit`。
    ///
    /// 写端要保持打开，直到宿主报告 CLI 已退出（host_exit）：先关写端，宿主会立刻放开这条连接，连接端随之退出，
    /// 这里就会在 CLI 还在收尾（关闭 MCP 服务、写会话记录）时以为它已经退出。用户的实例带着多个 MCP 服务，
    /// CLI 收尾要好几秒，编辑历史消息因此在回退前误报“原生会话仍在其他 CLI 中打开”（2026-11-25 实际发生）。
    async fn end_cli(&self, command: &str, limit: Duration) {
        let _lifecycle = self.lifecycle.lock().await;
        let cli = self.cli.lock().await.take();
        let Some(cli) = cli else {
            return;
        };
        let _ = cli.writer.send(Line {
            bytes: format!("{{\"type\":\"{command}\"}}\n").into_bytes(),
            done: None,
        });
        let wait = async {
            loop {
                let notified = self.exited.notified();
                if !self.alive() {
                    break;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(limit, wait).await.is_err() {
            drop(cli.guard);
        }
    }

    // ------------------------------------------------------------ 读取器

    async fn start_reader(self: &Arc<Self>) -> Result<Value, PeerError> {
        let mut guard = self.reader.lock().await;
        if guard.is_some() {
            drop(guard);
            return Ok(Value::Null);
        }
        let io = self
            .backend
            .spawn_reader(&self.spec.native_id)
            .await
            .map_err(|failure| {
                PeerError::plain(format!("无法启动 Claude 历史读取器：{failure}"), true)
            })?;
        let (writer, receiver) = mpsc::unbounded_channel();
        tokio::spawn(write_lines(receiver, io.writer));
        let generation = self.reader_generation.fetch_add(1, Ordering::AcqRel) + 1;
        *guard = Some(ReaderConn {
            writer,
            guard: io.guard,
            generation,
        });
        if let Some(stderr) = io.stderr {
            tokio::spawn(drain_stderr(stderr));
        }
        tokio::spawn(read_reader(self.clone(), generation, io.reader));
        drop(guard);
        // 读取器直接使用上层的初始化请求（目录、网页消息身份、待确认列表、原生控制状态）。
        let mut init = lock(&self.initialize_request).clone();
        init["subtype"] = json!("initialize");
        let response = self.reader_request(init).await.map_err(|failure| {
            PeerError::plain(
                format!("Claude 历史读取器初始化失败：{}", failure.message),
                true,
            )
        })?;
        Ok(response["contextConfiguration"].clone())
    }

    /// 读取器进程可能被外部结束（例如用户重启 WSL），而它只在建立连接时启动过一次：缺失时在使用前
    /// 按最新状态重新启动，不能让这条连接从此无法发送。CLI 连接、执行中的任务和待处理审批不受影响。
    async fn ensure_reader(self: &Arc<Self>) -> Result<(), ReaderFailure> {
        // 启动和初始化完成之前，后来的请求在这里等待，不能抢在初始化请求之前写给新读取器。
        let _restart = self.reader_restart.lock().await;
        if self.reader.lock().await.is_some()
            || self.closed.load(Ordering::Acquire)
            || lock(&self.initialize_request).is_null()
        {
            return Ok(());
        }
        let started = Instant::now();
        if let Err(failure) = self.start_reader().await {
            // 初始化失败的读取器不能留着当作可用：未初始化时它对同步请求直接答成功，却不读任何记录。
            self.reader.lock().await.take();
            diagnose(
                "reader_restart_failed",
                &self.spec.native_id,
                json!({"error":failure.message}),
            );
            return Err(ReaderFailure::local(&failure.message, true));
        }
        diagnose(
            "reader_restarted",
            &self.spec.native_id,
            json!({"ms":started.elapsed().as_millis() as u64}),
        );
        Ok(())
    }

    async fn reader_control(self: &Arc<Self>, request: Value) -> Result<Value, ReaderFailure> {
        self.ensure_reader().await?;
        self.reader_request(request).await
    }

    /// 只写给当前读取器，不触发重新启动：用于初始化本身，以及不应为它等待读取器启动的显示信号。
    async fn reader_request(&self, request: Value) -> Result<Value, ReaderFailure> {
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        lock(&self.reader_pending).insert(id.clone(), sender);
        let mut bytes = serde_json::to_vec(
            &json!({"type":"control_request","request_id":id,"request":request}),
        )
        .map_err(|failure| ReaderFailure::local(&failure.to_string(), true))?;
        bytes.push(b'\n');
        let sent = {
            let guard = self.reader.lock().await;
            match guard.as_ref() {
                Some(reader) => reader.writer.send(Line { bytes, done: None }).is_ok(),
                None => false,
            }
        };
        let outcome = if sent {
            match tokio::time::timeout(READER_CONTROL_TIMEOUT, receiver).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(ReaderFailure::local("Claude 历史读取器连接已关闭", false)),
                Err(_) => Err(ReaderFailure::local("Claude 历史读取器回执等待超时", false)),
            }
        } else {
            Err(ReaderFailure::local("Claude 历史读取器未启动", true))
        };
        lock(&self.reader_pending).remove(&id);
        outcome
    }

    async fn on_reader_frame(self: &Arc<Self>, frame: Value) {
        match frame["type"].as_str() {
            Some("control_response") => {
                let response = &frame["response"];
                let Some(id) = response["request_id"].as_str() else {
                    return;
                };
                if let Some(sender) = lock(&self.reader_pending).remove(id) {
                    let result = if response["subtype"] == "success" {
                        Ok(response["response"].clone())
                    } else {
                        Err(ReaderFailure {
                            code: response["code"].as_str().unwrap_or("").to_owned(),
                            message: response["error"].as_str().unwrap_or("读取失败").to_owned(),
                            not_sent: response["not_sent"].as_bool().unwrap_or(true),
                        })
                    };
                    let _ = sender.send(result);
                }
            }
            Some("shared_snapshot") => {
                self.remember_reader_state(&frame);
                let mut reader = lock(&self.reader_frame);
                *reader = Some(frame);
                self.reader_rev.fetch_add(1, Ordering::AcqRel);
                self.dirty.notify_one();
            }
            Some("shared_compaction_started") if frame["native_id"] == self.spec.native_id => {
                let mut reader = lock(&self.reader_frame);
                if let Some(cached) = reader.as_mut()
                    && super::super::native_controls::accept_live_start(&mut cached["native_control"], &frame["event"]) {
                    lock(&self.initialize_request)["native_control"] = cached["native_control"].clone();
                    self.reader_rev.fetch_add(1, Ordering::AcqRel);
                    // 在读取器发送全历史前，先透传已定位的原生开始；不在这里触发历史合成。
                    let _ = self.out.send(frame);
                }
            }
            // 派发意图与命令意图必须在对应写入之前到达上层，用于重启后核对而不是盲目重发。
            Some("shared_delivery_intent") => {
                if let Some(pending) = lock(&self.initialize_request)
                    .get_mut("pending")
                    .and_then(Value::as_array_mut)
                {
                    pending.push(frame["intent"].clone());
                }
                let _ = self.out.send(frame);
            }
            Some("shared_native_command_intent") => {
                self.remember_reader_state(&frame);
                let mut reader = lock(&self.reader_frame);
                if let Some(cached) = reader.as_mut() {
                    cached["native_control"] = frame["native_control"].clone();
                    self.reader_rev.fetch_add(1, Ordering::AcqRel);
                }
                let _ = self.out.send(frame);
            }
            _ => {}
        }
    }

    /// 记下读取器最新的网页消息身份、待确认记录和原生命令状态：重新启动读取器时按它恢复，
    /// 不能退回建立连接时的旧值，否则其间发出的消息无法再按原生记录核对。
    fn remember_reader_state(&self, frame: &Value) {
        let mut request = lock(&self.initialize_request);
        if !request.is_object() {
            return;
        }
        for key in ["clients", "pending", "native_control"] {
            if !frame[key].is_null() {
                request[key] = frame[key].clone();
            }
        }
    }

    async fn on_reader_exit(self: &Arc<Self>, generation: u64) {
        // 清理期间一直持有读取器锁：新的读取器装不上来，不会把它的请求一起按“已退出”结束。
        let mut reader = self.reader.lock().await;
        if reader
            .as_ref()
            .is_some_and(|current| current.generation != generation)
        {
            return;
        }
        let exited = reader.take().is_some();
        for (_, sender) in lock(&self.reader_pending).drain() {
            let _ = sender.send(Err(ReaderFailure::local("Claude 历史读取器已退出", false)));
        }
        // 上层释放会话时的正常收尾不记录。
        if exited && !self.closed.load(Ordering::Acquire) {
            diagnose("reader_exited", &self.spec.native_id, json!({}));
        }
    }

    // ------------------------------------------------------------ 具体操作

    async fn send(self: &Arc<Self>, request: &Value) -> Reply {
        let client_id = request["client_id"].as_str().unwrap_or("").to_owned();
        let content = request["content"].clone();
        if client_id.is_empty() || !content.is_array() {
            return Err(PeerError::new("EINVAL", "Claude 消息格式无效", true));
        }
        // 自动续发必须以实时状态为准：CLI 仍在执行或等待交互时继续等，不把下一条塞进原生队列。
        if request["idle_only"] == true && self.state(|state| state.busy()) {
            self.sync().await?;
            return Ok(json!({"deferred":true,"accepted":false}));
        }
        self.ensure_cli(false).await?;
        let uuid = uuid::Uuid::new_v4().to_string();
        let prepared = self
            .reader_control(json!({"subtype":"sdk_prepare","uuid":uuid,"client_id":client_id,"content":content}))
            .await
            .map_err(reader_error)?;
        let text = prepared["text"].as_str().unwrap_or("").to_owned();
        self.state(|state| state.record_sent(&uuid));
        let mut frame = json!({"type":"user","uuid":uuid,"parent_tool_use_id":null,
            "message":{"role":"user","content":[{"type":"text","text":text}]}});
        if request["steer"] == true {
            // “引导”：当前工具调用完成后吸收这条消息；本轮已结束则作为下一轮。
            frame["priority"] = json!("next");
        }
        match self.deliver(&uuid, &frame).await {
            Ok(()) => {
                lock(&self.launch).resumed = true;
                let _ = self
                    .reader_control(json!({"subtype":"sdk_delivered","uuid":uuid}))
                    .await;
                self.emit_now();
                Ok(json!({"nativeId":self.spec.native_id,"jobId":"","accepted":true}))
            }
            Err(failure) => {
                if failure.not_sent {
                    self.state(|state| state.forget_sent(&uuid));
                    let _ = self
                        .reader_control(json!({"subtype":"sdk_abandon","uuid":uuid}))
                        .await;
                    self.emit_now();
                }
                Err(failure)
            }
        }
    }

    /// 写入一条用户消息并等待 CLI 的 queued 回执。写入失败且明确未发出才算未投递；
    /// 已写入但没有回执只能算结果未知，保留意图供核对，绝不自动重发。
    async fn deliver(&self, uuid: &str, frame: &Value) -> Result<(), PeerError> {
        // 写入用户消息就不再是单纯的预热：恢复正常的空闲回收时间。
        self.warm_only.store(false, Ordering::Release);
        let (sender, receiver) = oneshot::channel();
        lock(&self.acks).insert(uuid.to_owned(), sender);
        if let Err(failure) = self.send_cli(frame).await {
            lock(&self.acks).remove(uuid);
            return Err(failure);
        }
        match tokio::time::timeout(ACK_TIMEOUT, receiver).await {
            Ok(Ok(())) => Ok(()),
            _ => {
                lock(&self.acks).remove(uuid);
                if self
                    .state(|state| state.sent_phase(uuid))
                    .is_some_and(|phase| phase >= Phase::Queued)
                {
                    return Ok(());
                }
                diagnose("ack_timeout", &self.spec.native_id, json!({"uuid":uuid}));
                Err(PeerError::plain(
                    "Claude 未确认收到消息，结果待核对，未自动重发",
                    false,
                ))
            }
        }
    }

    async fn native_command(self: &Arc<Self>, request: &Value) -> Reply {
        let name = request["name"].as_str().unwrap_or("").to_owned();
        let argument = request["argument"].as_str().unwrap_or("").trim().to_owned();
        // 命令状态由读取器按原有规则登记（与原生记录里的证据对应）；先登记，写入失败再回填结果。
        let mut registered = self
            .reader_control(
                json!({"subtype":"sdk_command","id":request["id"],"name":name,
                "argument":argument,"expectedGoal":request["expectedGoal"],
                "compactionModel":request["compactionModel"],"compactionModelSource":request["compactionModelSource"]}),
            )
            .await
            .map_err(reader_error)?;
        if let Err(failure) = self.ensure_cli(false).await {
            let _ = self
                .reader_control(
                    json!({"subtype":"sdk_command_result","id":request["id"],"state":"failed"}),
                )
                .await;
            return Err(failure);
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        let text = if argument.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {argument}")
        };
        self.state(|state| state.record_sent(&uuid));
        let frame = json!({"type":"user","uuid":uuid,"parent_tool_use_id":null,
            "message":{"role":"user","content":[{"type":"text","text":text}]}});
        if let Err(failure) = self.deliver(&uuid, &frame).await {
            if failure.not_sent {
                self.state(|state| state.forget_sent(&uuid));
            }
            let _ = self
                .reader_control(json!({"subtype":"sdk_command_result","id":request["id"],
                    "state":if failure.not_sent {"failed"} else {"unknown"}}))
                .await;
            return Err(failure);
        }
        lock(&self.launch).resumed = true;
        // 压缩意图已先交给上层，不用为“已受理”再排入一份旧历史；开始、终态由读取器照常通知。
        if name != "compact" { self.emit_now(); }
        registered["accepted"] = json!(true);
        registered["nativeId"] = json!(self.spec.native_id);
        registered["jobId"] = json!("");
        Ok(registered)
    }

    /// 编辑历史消息：回退到目标消息之前，再把修改后的内容作为新消息发送。
    ///
    /// 用户要求：只在完全空闲时允许；回退由原生 `rewind_conversation` 结构化接口完成，不模拟 /rewind 菜单按键；
    /// 回退前先让自己的 CLI 退出，使回退进程独占这份记录；失败时如实区分“未修改”和“结果未知”。
    async fn edit_message(self: &Arc<Self>, request: &Value) -> Reply {
        let client_id = request["client_id"].as_str().unwrap_or("").to_owned();
        if client_id.is_empty() {
            return Err(PeerError::new("EINVAL", "Claude 消息格式无效", true));
        }
        let prepared = self
            .reader_control(
                json!({"subtype":"sdk_edit_prepare","native_user_id":request["native_user_id"],
                "history_head":request["history_head"],"prompt":request["prompt"]}),
            )
            .await
            .map_err(reader_error)?;
        let (busy, background) = self.state(|state| (state.busy(), state.has_background_tasks()));
        if busy {
            return Err(PeerError::new(
                "EBUSY",
                "请等待 Claude 结束当前执行后再编辑历史消息",
                true,
            ));
        }
        if background {
            return Err(PeerError::new(
                "EBUSY",
                "Claude 仍有后台任务在运行；编辑会停止会话并结束这些任务，请等任务结束后再编辑",
                true,
            ));
        }
        if self.alive() {
            self.stop_cli().await;
            if self.alive() {
                return Err(PeerError::new(
                    "ESESSION",
                    "未能让 Claude 退出，未修改历史",
                    true,
                ));
            }
        }
        self.reader_control(json!({"subtype":"sdk_rewind","target":prepared["target"],
            "last_seen":prepared["last_seen"],"cwd":prepared["cwd"],"history_head":request["history_head"]}))
            .await
            .map_err(reader_error)?;
        // 回退已被原生确认：此后的失败都是“历史已回退，修改后的消息未发送”。
        {
            let mut selection = lock(&self.selection);
            if let Some(model) = request["model"].as_str() {
                selection.0 = model.to_owned();
            }
            if let Some(effort) = request["effort"].as_str() {
                selection.1 = effort.to_owned();
            }
        }
        let send = json!({"subtype":"shared_send","client_id":client_id,"content":prepared["content"],
            "steer":false,"idle_only":false});
        match self.send(&send).await {
            Ok(mut result) => {
                result["reverted"] = json!(true);
                Ok(result)
            }
            Err(failure) if failure.not_sent => Err(PeerError::new(
                "ESESSION",
                "历史已回退，但原生会话未能重新启动；修改后的消息未发送，可直接重新发送",
                true,
            )),
            Err(failure) => Err(failure),
        }
    }

    async fn answer(self: &Arc<Self>, request: &Value) -> Reply {
        let response = &request["response"];
        let Some(id) = response["request_id"].as_str() else {
            return Err(PeerError::new("EINVAL", "缺少原生请求标识", true));
        };
        let reply = self
            .state(|state| state.answer(id, &response["response"]))
            .map_err(|failure| match failure {
                AnswerError::Gone => PeerError::new("ESTALE", "该原生请求已经结束", true),
                AnswerError::Invalid(message) => PeerError::new("EINVAL", message, true),
            })?;
        let frame = json!({"type":"control_response","response":{"subtype":"success",
            "request_id":id,"response":reply}});
        self.send_cli(&frame).await?;
        let seen = self.state(|state| state.answered(id, now_ms()));
        self.apply(seen).await;
        self.emit_now();
        Ok(json!({"nativeSettings":self.settings_projection()}))
    }

    /// 停止当前执行。用户要求停止必须有确定的结果：不能再像 2026-11-25 那样等满 60 秒、
    /// 最后留下“送达状态未知”（那次 CLI 已在原生记录里写下中断标记，控制请求却一直没有回执）。
    ///
    /// 发出 `interrupt` 后，CLI 回执、或会话转为空闲（CLI 自己报告，或进程已退出）任一出现就算停止成功；
    /// 等满 `INTERRUPT_WAIT` 仍两者皆无，说明 CLI 没有响应，强制结束会话进程（下次发送会用 --resume 接上）。
    async fn interrupt(self: &Arc<Self>) -> Reply {
        if !self.alive() {
            return Ok(json!({"nativeSettings":self.settings_projection()}));
        }
        if self.state(|state| state.compacting()) {
            // 压缩被中断：读取器据此在压缩结束时记为“已停止”而不是“待核对”。
            let _ = self
                .reader_request(json!({"subtype":"sdk_signal","stopping":true}))
                .await;
        }
        // 写入失败说明停止请求明确没有送达，原样报告；已写入才进入下面的等待。
        let (id, receiver) = self.begin_cli_control("interrupt", json!({})).await?;
        let outcome = tokio::select! {
            response = receiver => Some(response),
            () = self.wait_idle() => None,
            () = tokio::time::sleep(INTERRUPT_WAIT) => {
                // 等待的最后一刻再看一眼，避免回执与超时恰好同时到达时误杀。
                if self.state(|state| state.busy()) { Some(Ok(Err("Claude 没有响应停止请求".to_owned()))) } else { None }
            }
        };
        lock(&self.cli_pending).remove(&id);
        let settled = match outcome {
            None | Some(Ok(Ok(_))) => true,
            // 连接关闭：进程已经退出，执行随之结束。
            Some(Err(_)) => !self.alive() || !self.state(|state| state.busy()),
            // CLI 明确拒绝，或等满仍没有回执：都不能让用户一直等下去。
            Some(Ok(Err(_))) => !self.state(|state| state.busy()),
        };
        if settled {
            return Ok(json!({"nativeSettings":self.settings_projection()}));
        }
        diagnose("interrupt_unresponsive", &self.spec.native_id, json!({}));
        self.kill_cli().await;
        if self.alive() {
            return Err(PeerError::new(
                "ESESSION",
                "Claude 没有响应停止请求，强制结束进程也未成功，请稍后重试",
                false,
            ));
        }
        Ok(json!({"nativeSettings":self.settings_projection(),"forced":true}))
    }

    async fn set_model(self: &Arc<Self>, request: &Value) -> Reply {
        let model = request["model"].as_str().unwrap_or("");
        lock(&self.selection).0 = model.to_owned();
        if self.alive() {
            let fields = if model.is_empty() {
                json!({})
            } else {
                json!({"model":model})
            };
            let response = self
                .cli_control("set_model", fields, CLI_CONTROL_TIMEOUT)
                .await
                .map_err(|message| PeerError::plain(message, false))?;
            if let Some(actual) = response["model"].as_str() {
                self.state(|state| state.actual.model = actual.to_owned());
                self.bump();
            }
        } else if !model.is_empty() {
            // CLI 尚未启动：下次启动后按这个值设置，当前只更新展示。
            self.state(|state| state.actual.model = model.to_owned());
            self.bump();
        }
        Ok(json!({"nativeSettings":self.settings_projection()}))
    }

    async fn apply_flag_settings(self: &Arc<Self>, request: &Value) -> Reply {
        let effort = request["settings"]["effortLevel"].as_str().unwrap_or("");
        lock(&self.selection).1 = effort.to_owned();
        if self.alive() {
            self.cli_control(
                "apply_flag_settings",
                json!({"settings":request["settings"]}),
                CLI_CONTROL_TIMEOUT,
            )
            .await
            .map_err(|message| PeerError::plain(message, false))?;
        }
        self.state(|state| state.actual.effort = effort.to_owned());
        self.bump();
        Ok(json!({"nativeSettings":self.settings_projection()}))
    }

    async fn set_permission_mode(self: &Arc<Self>, request: &Value) -> Reply {
        let requested = request["mode"].as_str().unwrap_or("").to_owned();
        if requested.is_empty() {
            return Err(PeerError::new("EINVAL", "Claude 权限模式无效", true));
        }
        let mode = if requested == "configured" {
            // “沿用实例配置”：由读取器按实例的 settings.json 解析出实际模式名。
            let resolved = self
                .reader_control(json!({"subtype":"sdk_configured_mode"}))
                .await
                .map_err(reader_error)?;
            resolved["mode"]
                .as_str()
                .map(normalize_mode)
                .unwrap_or_else(|| "default".to_owned())
        } else {
            normalize_mode(&requested)
        };
        let record = |state: &str, actual: Option<&str>, reason: Option<&str>| {
            let mut change = json!({"id":uuid::Uuid::new_v4().to_string(),"requested":requested,
                "issuedAt":now_ms(),"state":state});
            if let Some(actual) = actual {
                change["actual"] = json!(actual);
            }
            if let Some(reason) = reason {
                change["reason"] = json!(reason);
            }
            change
        };
        if !self.alive() {
            // CLI 尚未启动：下次启动用 --permission-mode 带上，立即生效。
            lock(&self.launch).permission_mode = Some(mode.clone());
            self.state(|state| state.actual.permission_mode = mode.clone());
            lock(&self.changes)["permissionMode"] = record("applied", Some(&mode), None);
            self.bump();
            return Ok(
                json!({"permissionPending":false,"nativeSettings":self.settings_projection()}),
            );
        }
        match self
            .cli_control(
                "set_permission_mode",
                json!({"mode":mode}),
                CLI_CONTROL_TIMEOUT,
            )
            .await
        {
            Ok(response) => {
                let actual = response["mode"].as_str().unwrap_or(&mode).to_owned();
                lock(&self.launch).permission_mode = Some(mode.clone());
                self.state(|state| state.actual.permission_mode = actual.clone());
                lock(&self.changes)["permissionMode"] = record("applied", Some(&actual), None);
                self.bump();
                Ok(json!({"permissionPending":false,"nativeSettings":self.settings_projection()}))
            }
            Err(_) if mode == "bypassPermissions" => {
                // 完全访问需要启动时带 --allow-dangerously-skip-permissions，运行中的 CLI 无法升级：
                // 登记为待生效，空闲时重启同一会话后应用；执行中的任务不受影响。
                lock(&self.launch).permission_mode = Some(mode.clone());
                lock(&self.changes)["permissionMode"] = record(
                    "pending",
                    None,
                    Some("完全访问需要在空闲时重新启动 Claude 后生效"),
                );
                self.bump();
                self.schedule_pending_permission();
                Ok(json!({"permissionPending":true,"nativeSettings":self.settings_projection()}))
            }
            Err(message) => Err(PeerError::plain(message, false)),
        }
    }

    /// 待生效的完全访问权限：会话空闲时重启 CLI 以带上启动参数。重启要等旧进程退出，
    /// 不能在 CLI 读取任务里同步等待，所以另起任务并保证同一时间只有一次。
    fn schedule_pending_permission(self: &Arc<Self>) {
        let pending = lock(&self.changes)["permissionMode"]["state"] == "pending";
        if !pending || !self.alive() || self.restarting.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = self.clone();
        tokio::spawn(async move {
            inner.restart_for_permission().await;
            inner.restarting.store(false, Ordering::Release);
        });
    }

    async fn restart_for_permission(self: &Arc<Self>) {
        let quiet = self.state(|state| !state.busy() && !state.has_background_tasks())
            && lock(&self.acks).is_empty();
        if !quiet {
            return;
        }
        self.stop_cli().await;
        if self.ensure_cli(false).await.is_err() {
            return;
        }
        let actual = lock(&self.launch)
            .permission_mode
            .clone()
            .unwrap_or_default();
        {
            let mut changes = lock(&self.changes);
            if changes["permissionMode"]["state"] == "pending" {
                changes["permissionMode"]["state"] = json!("applied");
                changes["permissionMode"]["actual"] = json!(actual);
                if let Some(change) = changes["permissionMode"].as_object_mut() {
                    change.remove("reason");
                }
            }
        }
        self.state(|state| state.actual.permission_mode = actual);
        self.bump();
    }

    async fn shutdown(self: &Arc<Self>) -> Reply {
        let (alive, busy, background) =
            self.state(|state| (state.alive(), state.busy(), state.has_background_tasks()));
        if !alive {
            return Ok(json!({"stopped":true}));
        }
        if busy {
            return Ok(json!({"stopped":false,"reason":"active"}));
        }
        if background {
            return Ok(json!({"stopped":false,"reason":"background_work"}));
        }
        self.stop_cli().await;
        Ok(if self.alive() {
            json!({"stopped":false,"reason":"state_changed"})
        } else {
            json!({"stopped":true})
        })
    }
}

/// CLI 接受的权限模式名：网页里的 manual 就是 CLI 的 default。
fn normalize_mode(mode: &str) -> String {
    if mode == "manual" {
        "default".to_owned()
    } else {
        mode.to_owned()
    }
}

fn reader_error(failure: ReaderFailure) -> PeerError {
    // 读取器的错误码沿用旧共享连接端的 NativeError 编码，上层按它映射状态码。
    const KNOWN: [&str; 9] = [
        "EBUSY",
        "EINVAL",
        "EIMAGE",
        "ESTALE",
        "ESESSION",
        "ESESSIONOPEN",
        "ENOENT",
        "EQUESTIONCHANGED",
        "EUNSUPPORTED",
    ];
    match KNOWN.iter().find(|known| **known == failure.code) {
        Some(code) => PeerError::new(code, failure.message, failure.not_sent),
        None => PeerError::plain(failure.message, failure.not_sent),
    }
}

async fn write_lines(
    mut receiver: mpsc::UnboundedReceiver<Line>,
    mut writer: Box<dyn AsyncWrite + Send + Unpin>,
) {
    while let Some(line) = receiver.recv().await {
        let result = async {
            writer.write_all(&line.bytes).await?;
            writer.flush().await
        }
        .await
        .map_err(|failure| failure.to_string());
        let failed = result.is_err();
        if let Some(done) = line.done {
            let _ = done.send(result);
        }
        if failed {
            break;
        }
    }
    // 发送端全部丢弃：关闭写端，对端看到标准输入结束后收尾。
    let _ = writer.shutdown().await;
}

async fn drain_stderr(stderr: Box<dyn AsyncRead + Send + Unpin>) {
    // stderr 只用于避免管道写满；内容可能含配置，不记录、不转发到网页。
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(_)) = lines.next_line().await {}
}

async fn read_cli(inner: Arc<Inner>, generation: u64, reader: Box<dyn AsyncRead + Send + Unpin>) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    while let Some(parsed) = read_json_line(&mut reader, &mut line).await {
        if let Some(frame) = parsed {
            if inner.cli_generation.load(Ordering::Acquire) != generation {
                continue;
            }
            inner.on_cli_frame(generation, frame).await;
        }
    }
    inner.on_cli_exit(generation).await;
}

async fn read_reader(
    inner: Arc<Inner>,
    generation: u64,
    reader: Box<dyn AsyncRead + Send + Unpin>,
) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    while let Some(parsed) = read_json_line(&mut reader, &mut line).await {
        if let Some(frame) = parsed {
            if inner.reader_generation.load(Ordering::Acquire) != generation { continue; }
            if frame["type"] == "shared_snapshot" {
                inner.reader_bytes.store(line.len(), Ordering::Release);
            }
            inner.on_reader_frame(frame).await;
        }
    }
    inner.on_reader_exit(generation).await;
}

/// 合并读取器快照与实时状态后发给上层；只在任一侧变化时发送。
async fn emit_loop(inner: Arc<Inner>) {
    loop {
        inner.dirty.notified().await;
        if inner.closed.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(SNAPSHOT_COALESCE).await;
        if !inner.emit_snapshot(false) { continue; }
        // 放宽下一份的最短间隔；其间的变化已经通过 dirty 通知记下，间隔结束后合并发出最新状态。
        tokio::time::sleep(snapshot_spacing(inner.reader_bytes.load(Ordering::Acquire))).await;
    }
}

async fn preview_loop(inner: Arc<Inner>) {
    loop {
        inner.preview_dirty.notified().await;
        if inner.closed.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(PREVIEW_INTERVAL).await;
        let basis = lock(&inner.reader_frame)
            .as_ref()
            .and_then(|frame| frame["preview_basis"].as_str().map(str::to_owned));
        let Some(basis) = basis else {
            continue;
        };
        let Some(frame) = inner.state(|state| state.preview_frame(&basis)) else {
            continue;
        };
        let key = format!("{basis}|{}|{}", frame["native_user_id"], frame["text"]);
        {
            let mut last = lock(&inner.last_preview);
            if *last == key {
                continue;
            }
            *last = key;
        }
        let _ = inner.out.send(frame);
    }
}

async fn idle_loop(inner: Arc<Inner>) {
    let period =
        (inner.spec.idle_stop / 4).clamp(Duration::from_millis(50), Duration::from_secs(30));
    loop {
        tokio::time::sleep(period).await;
        if inner.closed.load(Ordering::Acquire) {
            return;
        }
        let quiet = inner
            .state(|state| state.alive() && !state.busy() && !state.has_background_tasks())
            && lock(&inner.acks).is_empty();
        if !quiet {
            continue;
        }
        let idle_for = lock(&inner.activity).elapsed();
        let limit = if inner.warm_only.load(Ordering::Acquire) {
            WARM_IDLE_STOP.min(inner.spec.idle_stop)
        } else {
            inner.spec.idle_stop
        };
        if idle_for >= limit {
            // 空闲会话不长期占用一个 CLI 进程；下次发送用 --resume 接上同一个原生会话。
            inner.stop_cli().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    const NATIVE: &str = "11111111-1111-4111-8111-111111111111";

    type Script = Arc<dyn Fn(&Value) -> Vec<Value> + Send + Sync>;

    #[derive(Default)]
    struct Log {
        cli: StdMutex<Vec<Value>>,
        reader: StdMutex<Vec<Value>>,
        specs: StdMutex<Vec<CliSpec>>,
        /// 假 CLI 收到 host_stop / host_kill 的时刻，以及它真正退出（连接关闭）的时刻。
        stop_seen_at: StdMutex<Option<tokio::time::Instant>>,
        exited_at: StdMutex<Option<tokio::time::Instant>>,
        /// 假读取器收到 sdk_rewind 的时刻。
        rewound_at: StdMutex<Option<tokio::time::Instant>>,
        /// 为真时假读取器在 shared_sync 时不重发快照（真实读取器没有变化时的行为，见 claude_shared.py 的 skip_unchanged）。
        quiet_sync: StdMutex<bool>,
        /// 结束当前假读取器进程（模拟重启 WSL 等外部结束）。
        reader_stop: StdMutex<Option<oneshot::Sender<()>>>,
        /// 为真时假读取器拒绝初始化请求。
        refuse_reader_init: StdMutex<bool>,
    }

    impl Log {
        async fn stop_reader(&self) {
            let _ = lock(&self.reader_stop).take().unwrap().send(());
            // 让对端读到结束并完成清理。
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        fn reader_initializations(&self) -> Vec<Value> {
            lock(&self.reader)
                .iter()
                .filter(|request| request["subtype"] == "initialize")
                .cloned()
                .collect()
        }
    }

    struct Fake {
        log: Arc<Log>,
        cli: Script,
        /// attach_only 时是否存在仍在运行的宿主。
        host_exists: bool,
        /// CLI 一启动通道就关闭（模拟 CLI 无法启动）。
        refuse: bool,
        /// 宿主连接端在发出 host_hello 之前收到的内容会被丢弃（重现真实 WSL 联调中的缺陷）。
        lossy_hello: bool,
        /// 收到 host_stop / host_kill 后 CLI 还要收尾多久才真正退出（带多个 MCP 服务的实例要数秒）。
        stop_delay: Duration,
    }

    fn line(frame: &Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(frame).unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn success(id: &Value, response: Value) -> Value {
        json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}})
    }

    fn failure(id: &Value, message: &str) -> Value {
        json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":message}})
    }

    /// 贴近真实 CLI 的最小行为：控制请求成功回执；用户消息先回 queued，再进入 running。
    fn standard_cli(frame: &Value) -> Vec<Value> {
        match frame["type"].as_str() {
            Some("control_request") => {
                let id = &frame["request_id"];
                let request = &frame["request"];
                match request["subtype"].as_str().unwrap_or("") {
                    "initialize" => vec![success(id, json!({"commands":[],"models":[]}))],
                    "set_model" => vec![success(id, json!({"model":request["model"]}))],
                    "set_permission_mode" if request["mode"] == "bypassPermissions" => {
                        vec![failure(
                            id,
                            "Cannot set permission mode to bypassPermissions",
                        )]
                    }
                    "set_permission_mode" => vec![success(id, json!({"mode":request["mode"]}))],
                    _ => vec![success(id, json!({}))],
                }
            }
            Some("user") => vec![
                json!({"type":"command_lifecycle","command_uuid":frame["uuid"],"state":"queued"}),
                json!({"type":"system","subtype":"session_state_changed","state":"running"}),
            ],
            _ => vec![],
        }
    }

    #[test]
    fn snapshot_spacing_grows_with_frame_size_and_is_capped() {
        // 小会话的间隔不超过合并等待，行为不变。
        assert!(snapshot_spacing(0) <= SNAPSHOT_COALESCE);
        assert!(snapshot_spacing(400_000) <= SNAPSHOT_COALESCE);
        // 实测 19 MB 历史的下游处理约 1 秒，相邻两份快照至少隔开这么久。
        let large = snapshot_spacing(19 * 1024 * 1024);
        assert!(
            large > Duration::from_millis(900) && large < Duration::from_millis(1100),
            "{large:?}"
        );
        assert_eq!(snapshot_spacing(usize::MAX), SNAPSHOT_MAX_SPACING);
        assert!(
            SNAPSHOT_MAX_SPACING <= Duration::from_secs(5),
            "再大也不能让状态更新迟到太久"
        );
    }

    fn snapshot() -> Value {
        json!({"type":"shared_snapshot","native_id":NATIVE,"job_id":"","native_status":{"tempo":"idle"},
            "messages":[],"history_head":"h","retired_usage":[],"clients":{},"pending":[],"requests":{},
            "preview_basis":null,"updated_at":null,"context_configuration":{},
            "native_control":{"settings":{},"commands":[]}})
    }

    #[async_trait]
    impl Backend for Fake {
        async fn spawn_cli(&self, spec: &CliSpec) -> anyhow::Result<Io> {
            lock(&self.log.specs).push(spec.clone());
            let (writer, mut cli_in) = duplex(1 << 20);
            let (mut cli_out, reader) = duplex(1 << 20);
            if self.refuse {
                drop((cli_in, cli_out));
                return Ok(Io {
                    writer: Box::new(writer),
                    reader: Box::new(reader),
                    stderr: None,
                    guard: Box::new(()),
                });
            }
            let (script, log) = (self.cli.clone(), self.log.clone());
            let (attach_only, host_exists, lossy, stop_delay) = (
                spec.attach_only,
                self.host_exists,
                self.lossy_hello,
                self.stop_delay,
            );
            tokio::spawn(async move {
                if lossy {
                    // 重现真实宿主连接端的缺陷：host_hello 之前收到的内容会被丢掉（曾让 initialize 无人应答）。
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let mut scratch = [0u8; 65536];
                    let _ =
                        tokio::time::timeout(Duration::from_millis(10), cli_in.read(&mut scratch))
                            .await;
                }
                if attach_only && !host_exists {
                    let exit = json!({"type":"host_exit","code":null,"error":"none","stderr":""});
                    let _ = cli_out.write_all(&line(&exit)).await;
                    return;
                }
                // 真实宿主接上之后总是先发 host_hello，再转交后续帧。
                let hello = json!({"type":"host_hello","pid":1,"state":"idle"});
                let _ = cli_out.write_all(&line(&hello)).await;
                let mut lines = BufReader::new(cli_in).lines();
                // CLI 收尾结束的信号：真实宿主在 CLI 收尾期间保持连接，收尾结束才报告退出并断开。
                let (finished, mut cli_gone) = oneshot::channel::<()>();
                let mut finished = Some(finished);
                loop {
                    let next = tokio::select! {
                        next = lines.next_line() => next,
                        _ = &mut cli_gone => break,
                    };
                    // 对端关闭写端：真实的连接端会立刻放开这条连接（即使 CLI 还在收尾），这里同样立刻断开。
                    let Ok(Some(text)) = next else {
                        break;
                    };
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    lock(&log.cli).push(frame.clone());
                    if frame["type"] == "host_stop" || frame["type"] == "host_kill" {
                        *lock(&log.stop_seen_at) = Some(tokio::time::Instant::now());
                        if let Some(finished) = finished.take() {
                            let log = log.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(stop_delay).await;
                                *lock(&log.exited_at) = Some(tokio::time::Instant::now());
                                let _ = finished.send(());
                            });
                        }
                        continue;
                    }
                    for out in script(&frame) {
                        let _ = cli_out.write_all(&line(&out)).await;
                    }
                }
            });
            Ok(Io {
                writer: Box::new(writer),
                reader: Box::new(reader),
                stderr: None,
                guard: Box::new(()),
            })
        }

        async fn spawn_reader(&self, _native_id: &str) -> anyhow::Result<Io> {
            let (writer, reader_in) = duplex(1 << 20);
            let (mut reader_out, reader) = duplex(1 << 20);
            let log = self.log.clone();
            let (stop, mut stopped) = oneshot::channel();
            *lock(&log.reader_stop) = Some(stop);
            tokio::spawn(async move {
                let mut lines = BufReader::new(reader_in).lines();
                let mut command = Value::Null;
                loop {
                    let next = tokio::select! {
                        next = lines.next_line() => next,
                        _ = &mut stopped => break,
                    };
                    let Ok(Some(text)) = next else {
                        break;
                    };
                    let frame: Value = serde_json::from_str(&text).unwrap();
                    lock(&log.reader).push(frame["request"].clone());
                    let (id, request) = (&frame["request_id"], &frame["request"]);
                    let mut out = Vec::new();
                    match request["subtype"].as_str().unwrap_or("") {
                        "initialize" if *lock(&log.refuse_reader_init) => {
                            out.push(failure(id, "refused"));
                        }
                        "initialize" => {
                            out.push(snapshot());
                            out.push(success(id, json!({"contextConfiguration":{"source":"fake"}})));
                        }
                        "shared_sync" => {
                            if !*lock(&log.quiet_sync) {
                                out.push(snapshot());
                            }
                            out.push(success(id, json!({})));
                        }
                        "sdk_prepare" => {
                            let text = request["content"][0]["text"].as_str().unwrap_or("").to_owned();
                            out.push(json!({"type":"shared_delivery_intent","intent":{
                                "id":request["client_id"],"clientUserMessageId":request["client_id"],
                                "_nativeText":text,"_nativeUuid":request["uuid"],"deliveryConfirmed":false}}));
                            out.push(success(id, json!({"text":text})));
                        }
                        "sdk_command" => {
                            command = json!({"id":request["id"],"name":request["name"],"state":"pending"});
                            out.push(json!({"type":"shared_native_command_intent","native_control":{"commands":[command]}}));
                            out.push(success(
                                id,
                                json!({"nativeCommand":{"id":request["id"],"name":request["name"],"state":"pending"}}),
                            ));
                        }
                        "sdk_signal" if request["compacting"] == true && command["name"] == "compact" => {
                            out.push(json!({"type":"shared_compaction_started","native_id":NATIVE,
                                "event":{"id":command["id"],"source":"manual","status":"inProgress","afterUuid":"anchor"}}));
                            // 不提供后续历史快照，验证小帧能独立到达上层。
                            out.push(success(id,json!({})));
                        }
                        "sdk_configured_mode" => out.push(success(id, json!({"mode":"acceptEdits"}))),
                        "sdk_edit_prepare" => out.push(success(
                            id,
                            json!({"content":[{"type":"text","text":request["prompt"]}],"target":request["native_user_id"],
                                "last_seen":"last-1","cwd":"/work"}),
                        )),
                        "sdk_rewind" if request["target"] == "refuse" => out.push(failure(id, "refused")),
                        "sdk_rewind" => {
                            *lock(&log.rewound_at) = Some(tokio::time::Instant::now());
                            out.push(success(id, json!({"rewound":true})));
                        }
                        _ => out.push(success(id, json!({}))),
                    }
                    for frame in out {
                        let _ = reader_out.write_all(&line(&frame)).await;
                    }
                }
            });
            Ok(Io {
                writer: Box::new(writer),
                reader: Box::new(reader),
                stderr: None,
                guard: Box::new(()),
            })
        }
    }

    /// 扮演上层：按行收发，收集对端发来的全部帧。
    struct Upper {
        writer: Box<dyn AsyncWrite + Send + Unpin>,
        frames: Arc<StdMutex<Vec<Value>>>,
        arrived: Arc<Notify>,
        next: u64,
        log: Arc<Log>,
    }

    impl Upper {
        fn start(cli: Script, host_exists: bool, idle_stop: Duration) -> Self {
            Self::start_with(cli, host_exists, false, idle_stop)
        }

        fn start_with(cli: Script, host_exists: bool, refuse: bool, idle_stop: Duration) -> Self {
            Self::build(cli, host_exists, refuse, false, idle_stop)
        }

        fn build(
            cli: Script,
            host_exists: bool,
            refuse: bool,
            lossy_hello: bool,
            idle_stop: Duration,
        ) -> Self {
            Self::build_slow(
                cli,
                host_exists,
                refuse,
                lossy_hello,
                Duration::ZERO,
                idle_stop,
            )
        }

        fn build_slow(
            cli: Script,
            host_exists: bool,
            refuse: bool,
            lossy_hello: bool,
            stop_delay: Duration,
            idle_stop: Duration,
        ) -> Self {
            let log = Arc::new(Log::default());
            let backend = Arc::new(Fake {
                log: log.clone(),
                cli,
                host_exists,
                refuse,
                lossy_hello,
                stop_delay,
            });
            let (writer, reader) = super::super::connect_with(backend, NATIVE, false, idle_stop);
            let frames = Arc::new(StdMutex::new(Vec::new()));
            let arrived = Arc::new(Notify::new());
            let (collected, wake) = (frames.clone(), arrived.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(text)) = lines.next_line().await {
                    lock(&collected).push(serde_json::from_str(&text).unwrap());
                    wake.notify_waiters();
                }
            });
            Self {
                writer: Box::new(writer),
                frames,
                arrived,
                next: 0,
                log,
            }
        }

        fn standard() -> Self {
            Self::start(Arc::new(standard_cli), false, Duration::from_secs(600))
        }

        /// 发控制请求并返回 `response` 对象；回执之前到达的帧都在 `frames` 里，可检查先后顺序。
        async fn call(&mut self, request: Value) -> Value {
            self.next += 1;
            let id = format!("req-{}", self.next);
            self.writer
                .write_all(&line(
                    &json!({"type":"control_request","request_id":id,"request":request}),
                ))
                .await
                .unwrap();
            let frames = self.frames.clone();
            let arrived = self.arrived.clone();
            tokio::time::timeout(Duration::from_secs(120), async move {
                loop {
                    let notified = arrived.notified();
                    if let Some(found) = lock(&frames).iter().find(|frame| {
                        frame["type"] == "control_response" && frame["response"]["request_id"] == id
                    }) {
                        return found["response"].clone();
                    }
                    notified.await;
                }
            })
            .await
            .expect("对端应该答复控制请求")
        }

        async fn wait_for(&self, predicate: impl Fn(&Value) -> bool) -> Value {
            let (frames, arrived) = (self.frames.clone(), self.arrived.clone());
            tokio::time::timeout(Duration::from_secs(120), async move {
                loop {
                    let notified = arrived.notified();
                    if let Some(found) = lock(&frames).iter().find(|frame| predicate(frame)) {
                        return found.clone();
                    }
                    notified.await;
                }
            })
            .await
            .expect("应该收到对端的帧")
        }

        fn frames(&self) -> Vec<Value> {
            lock(&self.frames).clone()
        }

        fn cli_log(&self) -> Vec<Value> {
            lock(&self.log.cli).clone()
        }

        fn reader_log(&self) -> Vec<Value> {
            lock(&self.log.reader).clone()
        }

        async fn initialize(&mut self, extra: Value) -> Value {
            let mut request = json!({"subtype":"initialize","catalog":{"models":[],"commands":[],
                "permissionModes":["default","acceptEdits","plan","auto","bypassPermissions"]},
                "model":"","effort":"","permission_mode":"configured","attach":false,
                "clients":{},"pending":[],"native_control":{}});
            for (key, value) in extra.as_object().into_iter().flatten() {
                request[key] = value.clone();
            }
            self.call(request).await
        }

        fn position(&self, predicate: impl Fn(&Value) -> bool) -> Option<usize> {
            self.frames().iter().position(predicate)
        }
    }

    fn send_request(client: &str, text: &str) -> Value {
        json!({"subtype":"shared_send","client_id":client,"steer":false,"idle_only":false,
            "content":[{"type":"text","text":text}]})
    }

    /// 真实 WSL 联调中发现：宿主连接端会丢掉 host_hello 之前收到的内容，initialize 无人应答，白等 60 秒。
    /// 对端必须先等到 host_hello 再写第一条请求。
    #[tokio::test(start_paused = true)]
    async fn nothing_is_written_to_the_host_before_its_hello() {
        let mut upper = Upper::build(
            Arc::new(standard_cli),
            false,
            false,
            true,
            Duration::from_secs(600),
        );
        upper.initialize(json!({})).await;
        let response = upper.call(send_request("client-1", "你好")).await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert_eq!(
            upper.cli_log()[0]["request"]["subtype"],
            "initialize",
            "hello 之后才握手，没有丢帧"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn initialize_starts_the_reader_but_not_the_cli() {
        let mut upper = Upper::standard();
        let response = upper.initialize(json!({})).await;
        assert_eq!(response["subtype"], "success");
        assert_eq!(
            response["response"]["contextConfiguration"]["source"],
            "fake"
        );
        assert_eq!(upper.reader_log()[0]["subtype"], "initialize");
        assert!(upper.cli_log().is_empty(), "只读历史不启动 CLI");
        assert!(lock(&upper.log.specs).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn background_task_control_checks_run_before_writing_and_keeps_other_task_alive() {
        let mut upper=Upper::start(Arc::new(|frame|{
            if frame["type"]=="user"{
                let mut out=standard_cli(frame);
                out.push(json!({"type":"system","subtype":"init","claude_code_version":"2.1.330"}));
                for task in ["A","B"]{out.push(json!({"type":"system","subtype":"task_started","task_id":task,
                    "run_id":format!("run-{task}"),"is_backgrounded":true,"task_type":"local_bash"}));}
                return out;
            }
            let req=&frame["request"];
            match req["subtype"].as_str() {
                Some("get_task_output")=>vec![success(&frame["request_id"],json!({"output":
                    if req["task_id"]=="A" {" \r\nA [killed]\n"} else {" \r\nB\n"},"truncated":false}))],
                Some("stop_task")=>vec![success(&frame["request_id"],json!({})),json!({"type":"system",
                    "subtype":"task_notification","task_id":req["task_id"],"run_id":"run-A","status":"stopped"})],
                _=>standard_cli(frame)
            }
        }),false,Duration::from_secs(600));
        upper.initialize(json!({})).await;
        let call=|sub:&str,id:&str,run:&str|json!({"subtype":sub,"task_id":id,"run_id":run});
        assert_eq!(upper.call(call("stop_task","A","run-A")).await["subtype"],"error");
        upper.call(send_request("client-1","fixture")).await;
        upper.wait_for(|f|f["native_status"]["backgroundTaskItems"].as_array().is_some_and(|rs|rs.len()==2)).await;
        assert_eq!(upper.call(call("stop_task","B","run-A")).await["subtype"],"error");
        assert_eq!(upper.call(call("stop_task","A","run-A")).await["subtype"],"success");
        let output=upper.call(call("get_task_output","B","run-B")).await;
        assert_eq!(output["response"]["output"]," \r\nB\n");
        upper.wait_for(|f|f["background_task_results"].as_array().is_some_and(|rs|rs.iter().any(|r|
            r["taskId"]=="A" && r["output"]==" \r\nA [killed]\n"))).await;
        let sent=upper.cli_log();
        assert_eq!(sent.iter().filter(|f|f["request"]["subtype"]=="stop_task").count(),1);
        assert!(!sent.iter().any(|f|f["request"]["subtype"]=="interrupt"));
    }

    #[tokio::test(start_paused = true)]
    async fn send_starts_the_cli_and_confirms_only_after_the_queued_receipt() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        let response = upper.call(send_request("client-1", "你好")).await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert_eq!(response["response"]["accepted"], true);

        let cli = upper.cli_log();
        assert_eq!(
            cli[0]["request"]["subtype"], "initialize",
            "新启动的 CLI 先握手"
        );
        let user = cli
            .iter()
            .find(|frame| frame["type"] == "user")
            .expect("消息应写入 CLI");
        assert_eq!(user["message"]["content"][0]["text"], "你好");
        assert!(user.get("priority").is_none(), "普通消息不带 priority");
        let uuid = user["uuid"].as_str().unwrap();

        let reader: Vec<_> = upper
            .reader_log()
            .iter()
            .map(|r| r["subtype"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            &reader[reader.len() - 2..],
            ["sdk_prepare", "sdk_delivered"]
        );
        let intent = upper
            .position(|f| f["type"] == "shared_delivery_intent")
            .expect("派发意图应转交上层");
        let answer = upper
            .position(|f| f["type"] == "control_response" && f["response"]["request_id"] == "req-2")
            .unwrap();
        assert!(intent < answer, "意图必须先于回执到达上层");
        assert_eq!(upper.frames()[intent]["intent"]["_nativeUuid"], uuid);
    }

    #[tokio::test(start_paused = true)]
    async fn steer_messages_ask_the_cli_for_next_priority() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        let mut request = send_request("client-1", "改一下方向");
        request["steer"] = json!(true);
        assert_eq!(upper.call(request).await["subtype"], "success");
        let cli = upper.cli_log();
        let user = cli.iter().find(|frame| frame["type"] == "user").unwrap();
        assert_eq!(user["priority"], "next");
    }

    #[tokio::test(start_paused = true)]
    async fn missing_receipt_is_reported_as_unknown_and_never_resent() {
        // 假 CLI 收到消息后不回 queued：对端只能报告“结果待核对”，不能自动重发。
        let silent: Script = Arc::new(|frame| {
            if frame["type"] == "user" {
                vec![]
            } else {
                standard_cli(frame)
            }
        });
        let mut upper = Upper::start(silent, false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        let response = upper.call(send_request("client-1", "hi")).await;
        assert_eq!(response["subtype"], "error");
        assert_eq!(
            response["not_sent"], false,
            "已写入但未确认，不能当作未发送"
        );
        let writes = upper
            .cli_log()
            .iter()
            .filter(|frame| frame["type"] == "user")
            .count();
        assert_eq!(writes, 1);
        let reader: Vec<_> = upper
            .reader_log()
            .iter()
            .map(|r| r["subtype"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            !reader.contains(&"sdk_abandon".to_owned()),
            "未确认的意图必须保留供核对"
        );
        assert!(!reader.contains(&"sdk_delivered".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn failing_to_reach_the_cli_reports_not_sent_and_registers_no_intent() {
        let mut upper = Upper::start_with(
            Arc::new(standard_cli),
            false,
            true,
            Duration::from_secs(600),
        );
        upper.initialize(json!({})).await;
        let response = upper.call(send_request("client-1", "hi")).await;
        assert_eq!(response["subtype"], "error", "{response}");
        assert_eq!(response["not_sent"], true, "CLI 没有连上，消息一定没有发出");
        let reader: Vec<_> = upper
            .reader_log()
            .iter()
            .map(|r| r["subtype"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            !reader.contains(&"sdk_prepare".to_owned()),
            "没连上 CLI 就不登记派发意图"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn warming_starts_the_cli_without_sending_and_a_later_send_reuses_it() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        assert!(lock(&upper.log.specs).is_empty(), "初始化不启动 CLI");
        let reply = upper.call(json!({"subtype":"shared_warm"})).await;
        assert_eq!(reply["subtype"], "success", "{reply}");
        assert_eq!(reply["response"]["warm"], true);
        assert_eq!(lock(&upper.log.specs).len(), 1, "预热启动了一个 CLI");
        assert!(
            !upper.cli_log().iter().any(|frame| frame["type"] == "user"),
            "预热不能向 CLI 写任何用户消息"
        );
        assert_eq!(
            upper.call(send_request("client-1", "hi")).await["subtype"],
            "success"
        );
        assert_eq!(
            lock(&upper.log.specs).len(),
            1,
            "发送复用预热好的 CLI，不再启动第二个"
        );
        // 已经运行的 CLI 再预热：没有新启动，也不算预热。
        let again = upper.call(json!({"subtype":"shared_warm"})).await;
        assert_eq!(again["response"]["warm"], false);
        assert_eq!(lock(&upper.log.specs).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_warmed_cli_that_is_never_used_is_reclaimed_early() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        upper.call(json!({"subtype":"shared_warm"})).await;
        tokio::time::sleep(WARM_IDLE_STOP - Duration::from_secs(40)).await;
        assert!(lock(&upper.log.stop_seen_at).is_none(), "预热期限内不回收");
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(
            lock(&upper.log.stop_seen_at).is_some(),
            "预热后一直没有发送消息，2 分钟左右就收回，不占用 10 分钟"
        );
    }

    /// 收到用户消息后立即回答完并回到空闲，空闲回收才会计时。
    fn answering_cli(frame: &Value) -> Vec<Value> {
        let mut out = standard_cli(frame);
        if frame["type"] == "user" {
            out.push(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn sending_after_warming_restores_the_normal_idle_time() {
        let mut upper = Upper::start(Arc::new(answering_cli), false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        upper.call(json!({"subtype":"shared_warm"})).await;
        assert_eq!(
            upper.call(send_request("client-1", "hi")).await["subtype"],
            "success"
        );
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert!(
            lock(&upper.log.stop_seen_at).is_none(),
            "发送过消息的 CLI 按正常的 10 分钟回收，不能被预热的短期限收走"
        );
        tokio::time::sleep(Duration::from_secs(400)).await;
        assert!(lock(&upper.log.stop_seen_at).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn warming_a_running_cli_does_not_shorten_its_idle_time() {
        let mut upper = Upper::start(Arc::new(answering_cli), false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        upper.call(send_request("client-1", "hi")).await;
        upper.call(json!({"subtype":"shared_warm"})).await;
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert!(lock(&upper.log.stop_seen_at).is_none());
        tokio::time::sleep(Duration::from_secs(400)).await;
        assert!(
            lock(&upper.log.stop_seen_at).is_some(),
            "已经在运行的 CLI 仍按原来的 10 分钟回收"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn warming_reports_a_cli_that_cannot_start() {
        let mut upper = Upper::start_with(
            Arc::new(standard_cli),
            false,
            true,
            Duration::from_secs(600),
        );
        upper.initialize(json!({})).await;
        let reply = upper.call(json!({"subtype":"shared_warm"})).await;
        assert_eq!(reply["subtype"], "error", "{reply}");
        assert_eq!(reply["not_sent"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_only_sends_wait_while_the_cli_is_busy() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "first")).await["subtype"],
            "success"
        );
        let mut next = send_request("client-2", "second");
        next["idle_only"] = json!(true);
        let response = upper.call(next).await;
        assert_eq!(response["response"]["deferred"], true);
        assert_eq!(
            upper
                .cli_log()
                .iter()
                .filter(|frame| frame["type"] == "user")
                .count(),
            1
        );
    }

    /// 收到包含 RUN 的消息后，假 CLI 进入执行并请求一次工具审批。
    fn approval_cli(frame: &Value) -> Vec<Value> {
        let mut out = standard_cli(frame);
        if frame["type"] == "user" && frame["message"]["content"][0]["text"] == "RUN" {
            out.push(
                json!({"type":"control_request","request_id":"tool-1","request":{
                "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"touch a"},
                "tool_use_id":"toolu_1","description":"create a file"}}),
            );
            out.push(json!({"type":"system","subtype":"session_state_changed","state":"requires_action"}));
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn approvals_are_listed_in_the_snapshot_and_answers_reach_the_cli() {
        let mut upper = Upper::start(Arc::new(approval_cli), false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "RUN")).await["subtype"],
            "success"
        );
        let waiting = upper
            .wait_for(|frame| {
                frame["type"] == "shared_snapshot"
                    && frame["native_status"]["sessionStatus"] == "waiting"
            })
            .await;
        assert_eq!(waiting["native_status"]["needs"], "permission prompt");
        assert_eq!(waiting["requests"]["tool-1"]["tool_name"], "Bash");

        let answer = json!({"subtype":"shared_answer","response":{"subtype":"success","request_id":"tool-1",
            "response":{"behavior":"allow","updatedInput":{"command":"touch a"}}}});
        assert_eq!(upper.call(answer.clone()).await["subtype"], "success");
        let reply = upper
            .cli_log()
            .into_iter()
            .find(|frame| {
                frame["type"] == "control_response" && frame["response"]["request_id"] == "tool-1"
            })
            .expect("审批回复应写给 CLI");
        assert_eq!(reply["response"]["response"]["behavior"], "allow");
        assert_eq!(reply["response"]["response"]["toolUseID"], "toolu_1");

        // 同一请求不能回答第二次。
        let again = upper.call(answer).await;
        assert_eq!(again["subtype"], "error");
        assert_eq!(again["code"], "ESTALE");
    }

    #[tokio::test(start_paused = true)]
    async fn shared_sync_publishes_the_snapshot_before_its_receipt() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        let before = upper.frames().len();
        let response = upper.call(json!({"subtype":"shared_sync"})).await;
        assert_eq!(response["subtype"], "success");
        let frames = upper.frames();
        let receipt = frames
            .iter()
            .rposition(|f| f["type"] == "control_response")
            .unwrap();
        let snapshot = frames[before..receipt]
            .iter()
            .rposition(|f| f["type"] == "shared_snapshot");
        assert!(snapshot.is_some(), "上层必须先看到最新快照再收到回执");
        assert_eq!(
            frames[before + snapshot.unwrap()]["native_status"]["sessionAlive"],
            false
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shared_sync_does_not_resend_an_unchanged_snapshot_but_still_publishes_changes() {
        // 用户要求缩短发送前的等待：没有变化时上层手里的快照就是最新的，不再为它复制、序列化并重发整份历史。
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        upper.wait_for(|f| f["type"] == "shared_snapshot").await;
        let snapshots = |upper: &Upper| {
            upper
                .frames()
                .iter()
                .filter(|f| f["type"] == "shared_snapshot")
                .count()
        };
        *lock(&upper.log.quiet_sync) = true;
        let settled = snapshots(&upper);
        let response = upper.call(json!({"subtype":"shared_sync"})).await;
        assert_eq!(response["subtype"], "success");
        assert_eq!(snapshots(&upper), settled, "读取器和实时状态都没变，不重发");

        // 读取器有了新内容（假读取器重发一份快照）：回执之前上层必须已经看到。
        *lock(&upper.log.quiet_sync) = false;
        let response = upper.call(json!({"subtype":"shared_sync"})).await;
        assert_eq!(response["subtype"], "success");
        let frames = upper.frames();
        let receipt = frames
            .iter()
            .rposition(|f| f["type"] == "control_response")
            .unwrap();
        let latest = frames
            .iter()
            .rposition(|f| f["type"] == "shared_snapshot")
            .unwrap();
        assert!(snapshots(&upper) > settled, "有变化时发出新的快照");
        assert!(latest < receipt, "回执之前上层必须已经看到最新快照");
    }

    #[tokio::test(start_paused = true)]
    async fn an_exited_reader_is_restarted_with_the_latest_state_before_it_is_used_again() {
        // 重启过 WSL 之后读取器进程不在了：下一次发送前的同步要重新启动它，而不是让这条连接从此无法发送。
        let mut upper = Upper::standard();
        upper.initialize(json!({"clients":{"stale":"value"}})).await;
        upper.call(json!({"subtype":"shared_sync"})).await;
        let sent = upper.call(send_request("client-1", "第一条")).await;
        assert_eq!(sent["subtype"], "success");
        assert_eq!(
            upper.log.reader_initializations().len(),
            1,
            "读取器正常时不重启"
        );

        upper.log.stop_reader().await;
        let response = upper.call(json!({"subtype":"shared_sync"})).await;
        assert_eq!(response["subtype"], "success", "{response}");
        let initializations = upper.log.reader_initializations();
        assert_eq!(initializations.len(), 2);
        // 按读取器最后报告的状态恢复，不退回建立连接时的旧值；快照之后的派发意图也不丢。
        assert_eq!(initializations[1]["clients"], json!({}));
        assert_eq!(
            initializations[1]["native_control"],
            snapshot()["native_control"]
        );
        assert_eq!(
            initializations[1]["pending"][0]["clientUserMessageId"],
            "client-1"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn typing_warms_the_session_and_also_restarts_an_exited_reader() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        upper.log.stop_reader().await;
        let response = upper.call(json!({"subtype":"shared_warm"})).await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert_eq!(upper.log.reader_initializations().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_cannot_restart_reports_why_and_is_retried_on_the_next_request() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        upper.log.stop_reader().await;
        *lock(&upper.log.refuse_reader_init) = true;
        let response = upper.call(json!({"subtype":"shared_sync"})).await;
        assert_eq!(response["subtype"], "error");
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .starts_with("Claude 历史读取器初始化失败"),
            "{response}"
        );
        // 初始化失败的读取器不能当作可用：下一次请求重新启动，而不是向它同步后答成功。
        *lock(&upper.log.refuse_reader_init) = false;
        let response = upper.call(json!({"subtype":"shared_sync"})).await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert_eq!(upper.log.reader_initializations().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn model_and_effort_chosen_before_the_cli_starts_are_applied_after_it_starts() {
        let mut upper = Upper::standard();
        upper
            .initialize(json!({"model":"opus","effort":"high"}))
            .await;
        let response = upper
            .call(json!({"subtype":"set_model","model":"sonnet"}))
            .await;
        assert_eq!(
            response["response"]["nativeSettings"]["model"], "sonnet",
            "未启动时只更新展示"
        );
        assert!(upper.cli_log().is_empty());
        assert_eq!(
            upper.call(send_request("client-1", "hi")).await["subtype"],
            "success"
        );
        let subtypes: Vec<_> = upper
            .cli_log()
            .iter()
            .filter(|frame| frame["type"] == "control_request")
            .map(|frame| frame["request"]["subtype"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(subtypes, ["initialize", "set_model", "apply_flag_settings"]);
        let model = upper
            .cli_log()
            .into_iter()
            .find(|f| f["request"]["subtype"] == "set_model")
            .unwrap();
        assert_eq!(
            model["request"]["model"], "sonnet",
            "CLI 启动后恢复网页里最新选择的模型"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bypass_permission_is_pending_while_busy_and_restarts_the_cli_when_idle() {
        let idle_after_run: Script = Arc::new(|frame| {
            let mut out = standard_cli(frame);
            if frame["type"] == "control_request" && frame["request"]["subtype"] == "interrupt" {
                out.push(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
            }
            out
        });
        let mut upper = Upper::start(idle_after_run, false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "work")).await["subtype"],
            "success"
        );
        // 会话正在执行：完全访问只登记，不重启。
        let response = upper
            .call(json!({"subtype":"set_permission_mode","mode":"bypassPermissions"}))
            .await;
        assert_eq!(response["response"]["permissionPending"], true);
        assert_eq!(
            response["response"]["nativeSettings"]["changes"]["permissionMode"]["state"],
            "pending"
        );
        assert_eq!(
            lock(&upper.log.specs).len(),
            1,
            "执行中不能为了切换权限重启 CLI"
        );

        // 执行结束后重启，并在新进程的启动参数里带上完全访问。
        upper.call(json!({"subtype":"interrupt"})).await;
        let applied = upper
            .wait_for(|frame| {
                frame["type"] == "shared_snapshot"
                    && frame["native_control"]["settings"]["changes"]["permissionMode"]["state"]
                        == "applied"
            })
            .await;
        assert_eq!(
            applied["native_control"]["settings"]["permissionMode"],
            "bypassPermissions"
        );
        let specs = lock(&upper.log.specs).clone();
        assert_eq!(specs.len(), 2);
        assert_eq!(
            specs[1].permission_mode.as_deref(),
            Some("bypassPermissions")
        );
        assert!(specs[1].resumed, "重启后必须用 --resume 接上同一个原生会话");
        assert!(
            upper
                .cli_log()
                .iter()
                .any(|frame| frame["type"] == "host_stop")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn manual_and_configured_permission_modes_are_translated_for_the_cli() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        // 未启动：记入下次启动参数。
        let response = upper
            .call(json!({"subtype":"set_permission_mode","mode":"manual"}))
            .await;
        assert_eq!(
            response["response"]["nativeSettings"]["permissionMode"],
            "default"
        );
        let response = upper
            .call(json!({"subtype":"set_permission_mode","mode":"configured"}))
            .await;
        assert_eq!(
            response["response"]["nativeSettings"]["permissionMode"], "acceptEdits",
            "沿用实例配置的模式"
        );
        assert_eq!(
            upper.call(send_request("client-1", "hi")).await["subtype"],
            "success"
        );
        assert_eq!(
            lock(&upper.log.specs)[0].permission_mode.as_deref(),
            Some("acceptEdits")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn attaching_without_a_host_keeps_the_session_idle_and_does_not_start_a_cli() {
        let mut upper = Upper::start(Arc::new(standard_cli), false, Duration::from_secs(600));
        let response = upper.initialize(json!({"attach":true})).await;
        assert_eq!(response["subtype"], "success", "没有宿主不是错误");
        let spec = lock(&upper.log.specs)[0].clone();
        assert!(spec.attach_only, "恢复连接只接入已有宿主");
        let sync = upper.call(json!({"subtype":"shared_sync"})).await;
        assert_eq!(sync["subtype"], "success");
        let latest = upper
            .frames()
            .into_iter()
            .rev()
            .find(|f| f["type"] == "shared_snapshot")
            .unwrap();
        assert_eq!(latest["native_status"]["sessionAlive"], false);
    }

    #[tokio::test(start_paused = true)]
    async fn attaching_to_a_running_host_resumes_the_live_state() {
        let replaying: Script = Arc::new(|frame| {
            let mut out = standard_cli(frame);
            if frame["type"] == "control_request" && frame["request"]["subtype"] == "initialize" {
                out.push(
                    json!({"type":"system","subtype":"session_state_changed","state":"running"}),
                );
            }
            out
        });
        let mut upper = Upper::start(replaying, true, Duration::from_secs(600));
        upper.initialize(json!({"attach":true})).await;
        let running = upper
            .wait_for(|f| {
                f["type"] == "shared_snapshot" && f["native_status"]["sessionStatus"] == "busy"
            })
            .await;
        assert_eq!(running["native_status"]["sessionAlive"], true);
        assert_eq!(running["native_status"]["tempo"], "active");
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_cli_is_stopped_after_the_idle_period_and_restarted_with_resume() {
        // 假 CLI 立刻跑完这一轮：消息被接收、执行、回到空闲。
        let finishing: Script = Arc::new(|frame| {
            let mut out = standard_cli(frame);
            if frame["type"] == "user" {
                out.push(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
            }
            out
        });
        let mut upper = Upper::start(finishing, false, Duration::from_secs(60));
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "hi")).await["subtype"],
            "success"
        );
        assert_eq!(lock(&upper.log.specs).len(), 1);
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(
            upper.cli_log().iter().any(|f| f["type"] == "host_stop"),
            "空闲超时后请求宿主收尾"
        );
        // 下一条消息重新启动 CLI，并用 --resume 接上同一个原生会话。
        assert_eq!(
            upper.call(send_request("client-2", "again")).await["subtype"],
            "success"
        );
        let specs = lock(&upper.log.specs).clone();
        assert_eq!(specs.len(), 2);
        assert!(specs[1].resumed);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_refuses_while_active_and_stops_the_cli_when_idle() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        let idle = upper.call(json!({"subtype":"shared_shutdown"})).await;
        assert_eq!(idle["response"]["stopped"], true, "没有 CLI 时视为已停止");
        assert_eq!(
            upper.call(send_request("client-1", "hi")).await["subtype"],
            "success"
        );
        let busy = upper.call(json!({"subtype":"shared_shutdown"})).await;
        assert_eq!(busy["response"]["stopped"], false);
        assert_eq!(busy["response"]["reason"], "active");
    }

    #[tokio::test(start_paused = true)]
    async fn native_commands_are_registered_then_written_as_slash_text() {
        let mut upper = Upper::start(Arc::new(|frame| {
            let mut frames = standard_cli(frame);
            if frame["type"] == "user" {
                frames.push(json!({"type":"system","subtype":"status","status":"compacting"}));
            }
            frames
        }), false, Duration::from_secs(60));
        upper.initialize(json!({})).await;
        let response = upper
            .call(json!({"subtype":"shared_native_command","id":"cmd-1","name":"compact","argument":"focus","expectedGoal":null}))
            .await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert_eq!(response["response"]["accepted"], true);
        assert_eq!(response["response"]["nativeCommand"]["id"], "cmd-1");
        let user = upper
            .cli_log()
            .into_iter()
            .find(|frame| frame["type"] == "user")
            .unwrap();
        assert_eq!(user["message"]["content"][0]["text"], "/compact focus");
        let registered = upper
            .position(|f| f["type"] == "shared_native_command_intent")
            .unwrap();
        let receipt = upper
            .position(|f| f["type"] == "control_response" && f["response"]["request_id"] == "req-2")
            .unwrap();
        assert!(registered < receipt);
        let started = upper.wait_for(|f| f["type"] == "shared_compaction_started").await;
        assert_eq!(started["event"]["id"],"cmd-1");
        assert!(started.get("messages").is_none());
    }

    /// 假 CLI 立刻跑完这一轮：消息被接收、执行、回到空闲。
    fn finishing_cli(frame: &Value) -> Vec<Value> {
        let mut out = standard_cli(frame);
        if frame["type"] == "user" {
            out.push(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
        }
        out
    }

    fn edit_request(prompt: &str) -> Value {
        json!({"subtype":"shared_edit_message","client_id":"edit-1","native_user_id":"u-1",
            "history_head":"h","prompt":prompt,"model":"sonnet","effort":"high"})
    }

    #[tokio::test(start_paused = true)]
    async fn editing_stops_the_cli_rewinds_the_history_and_resends_the_new_text() {
        let mut upper = Upper::start(Arc::new(finishing_cli), false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "first")).await["subtype"],
            "success"
        );
        upper
            .wait_for(|f| f["type"] == "shared_snapshot" && f["native_status"]["tempo"] == "idle")
            .await;

        let response = upper.call(edit_request("改后的内容")).await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert_eq!(response["response"]["reverted"], true);
        let cli = upper.cli_log();
        let stop = cli
            .iter()
            .position(|f| f["type"] == "host_stop")
            .expect("回退前必须先让自己的 CLI 退出");
        let resend = cli.iter().rposition(|f| f["type"] == "user").unwrap();
        assert!(stop < resend);
        assert_eq!(cli[resend]["message"]["content"][0]["text"], "改后的内容");
        let reader: Vec<_> = upper
            .reader_log()
            .iter()
            .map(|r| r["subtype"].as_str().unwrap().to_owned())
            .collect();
        let prepare = reader.iter().position(|r| r == "sdk_edit_prepare").unwrap();
        let rewind = reader.iter().position(|r| r == "sdk_rewind").unwrap();
        assert!(prepare < rewind);
        let specs = lock(&upper.log.specs).clone();
        assert_eq!(specs.len(), 2);
        assert!(specs[1].resumed, "回退后用 --resume 接上同一个原生会话");
        // 编辑时选择的模型与思考深度在新进程里生效。
        let model = cli
            .iter()
            .rev()
            .find(|f| f["request"]["subtype"] == "set_model")
            .unwrap();
        assert_eq!(model["request"]["model"], "sonnet");
    }

    #[tokio::test(start_paused = true)]
    async fn editing_is_refused_while_the_session_is_working() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "first")).await["subtype"],
            "success"
        );
        let response = upper.call(edit_request("x")).await;
        assert_eq!(response["subtype"], "error");
        assert_eq!(response["code"], "EBUSY");
        assert_eq!(response["not_sent"], true);
        let reader: Vec<_> = upper
            .reader_log()
            .iter()
            .map(|r| r["subtype"].as_str().unwrap().to_owned())
            .collect();
        assert!(!reader.contains(&"sdk_rewind".to_owned()), "执行中不能回退");
        assert!(!upper.cli_log().iter().any(|f| f["type"] == "host_stop"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_rewind_leaves_history_untouched_and_sends_nothing() {
        let mut upper = Upper::start(Arc::new(finishing_cli), false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "first")).await["subtype"],
            "success"
        );
        upper
            .wait_for(|f| f["type"] == "shared_snapshot" && f["native_status"]["tempo"] == "idle")
            .await;
        let mut request = edit_request("x");
        request["native_user_id"] = json!("refuse");
        let response = upper.call(request).await;
        assert_eq!(response["subtype"], "error", "{response}");
        assert_eq!(response["not_sent"], true, "原生明确拒绝：历史未修改");
        assert_eq!(
            upper
                .cli_log()
                .iter()
                .filter(|f| f["type"] == "user")
                .count(),
            1,
            "修改后的消息不能发出"
        );
        assert_eq!(
            lock(&upper.log.specs).len(),
            1,
            "回退失败不重启 CLI，下次发送时再按需启动"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_upper_link_closes_the_cli_connection_without_killing_work() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "hi")).await["subtype"],
            "success"
        );
        let log = upper.log.clone();
        drop(upper);
        tokio::time::sleep(Duration::from_secs(1)).await;
        // 上层释放会话只是断开连接：不向宿主发 host_stop，执行中的任务由宿主继续。
        assert!(
            !lock(&log.cli)
                .iter()
                .any(|frame| frame["type"] == "host_stop")
        );
    }

    /// 带多个 MCP 服务的实例，CLI 收到停止后还要收尾数秒（2026-11-25 用户实机：编辑历史消息报“原生会话仍在其他 CLI 中打开”）。
    /// 回退前必须等 CLI 真正退出，否则读取器核对持有者时仍会看到它的登记。
    #[tokio::test(start_paused = true)]
    async fn editing_waits_until_the_cli_has_really_exited_before_rewinding() {
        let mut upper = Upper::build_slow(
            Arc::new(finishing_cli),
            false,
            false,
            false,
            Duration::from_secs(7),
            Duration::from_secs(600),
        );
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "first")).await["subtype"],
            "success"
        );
        upper
            .wait_for(|f| f["type"] == "shared_snapshot" && f["native_status"]["tempo"] == "idle")
            .await;

        let response = upper.call(edit_request("改后的内容")).await;
        assert_eq!(response["subtype"], "success", "{response}");
        let stop_seen = lock(&upper.log.stop_seen_at).expect("CLI 应收到停止请求");
        let exited = lock(&upper.log.exited_at).expect("CLI 应已退出");
        let rewound = lock(&upper.log.rewound_at).expect("应已请求回退");
        assert!(
            exited - stop_seen >= Duration::from_secs(7),
            "假 CLI 收尾 7 秒"
        );
        assert!(
            rewound >= exited,
            "必须等 CLI 真正退出才能回退：退出 {exited:?}，回退 {rewound:?}"
        );
    }

    /// CLI 一直不响应停止请求（既不回执也不转为空闲）：不能让用户等 60 秒后落到“未知”，
    /// 要限时强制结束进程，并明确告知这是强制停止。
    #[tokio::test(start_paused = true)]
    async fn an_unresponsive_cli_is_force_stopped_with_a_definite_result() {
        let silent: Script = Arc::new(|frame| {
            if frame["type"] == "control_request" && frame["request"]["subtype"] == "interrupt" {
                return vec![];
            }
            standard_cli(frame)
        });
        let mut upper = Upper::start(silent, false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "work")).await["subtype"],
            "success"
        );
        let started = tokio::time::Instant::now();
        let response = upper.call(json!({"subtype":"interrupt"})).await;
        let took = started.elapsed();
        assert_eq!(response["subtype"], "success", "{response}");
        assert_eq!(response["response"]["forced"], true);
        assert!(
            took >= INTERRUPT_WAIT && took < INTERRUPT_WAIT + FORCED_EXIT,
            "等满 INTERRUPT_WAIT 才强制，且在 FORCED_EXIT 内完成：{took:?}"
        );
        assert!(
            upper
                .cli_log()
                .iter()
                .any(|frame| frame["type"] == "host_kill"),
            "必须向宿主发出强制结束"
        );
        // 进程已结束：会话回到空闲，下一次发送用 --resume 接上原会话。
        upper
            .wait_for(|f| f["type"] == "shared_snapshot" && f["native_status"]["tempo"] == "idle")
            .await;
        assert_eq!(
            upper.call(send_request("client-2", "again")).await["subtype"],
            "success"
        );
        let specs = lock(&upper.log.specs).clone();
        assert_eq!(specs.len(), 2);
        assert!(specs[1].resumed);
    }

    /// CLI 明确拒绝停止请求且仍在执行：同样要强制结束，而不是只给一个错误提示让用户无计可施。
    #[tokio::test(start_paused = true)]
    async fn a_cli_that_refuses_to_interrupt_is_force_stopped() {
        let refusing: Script = Arc::new(|frame| {
            if frame["type"] == "control_request" && frame["request"]["subtype"] == "interrupt" {
                return vec![failure(&frame["request_id"], "cannot interrupt")];
            }
            standard_cli(frame)
        });
        let mut upper = Upper::start(refusing, false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "work")).await["subtype"],
            "success"
        );
        let response = upper.call(json!({"subtype":"interrupt"})).await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert_eq!(response["response"]["forced"], true);
        assert!(
            upper
                .cli_log()
                .iter()
                .any(|frame| frame["type"] == "host_kill")
        );
    }

    /// 回执没有到达、但 CLI 已自行报告空闲：停止已经生效，直接成功，不强制结束。
    #[tokio::test(start_paused = true)]
    async fn an_interrupt_is_done_once_the_session_reports_idle_even_without_a_receipt() {
        let idle_only: Script = Arc::new(|frame| {
            if frame["type"] == "control_request" && frame["request"]["subtype"] == "interrupt" {
                return vec![
                    json!({"type":"system","subtype":"session_state_changed","state":"idle"}),
                ];
            }
            standard_cli(frame)
        });
        let mut upper = Upper::start(idle_only, false, Duration::from_secs(600));
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "work")).await["subtype"],
            "success"
        );
        let started = tokio::time::Instant::now();
        let response = upper.call(json!({"subtype":"interrupt"})).await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert!(response["response"]["forced"].is_null(), "{response}");
        assert!(started.elapsed() < INTERRUPT_WAIT);
        assert!(
            !upper
                .cli_log()
                .iter()
                .any(|frame| frame["type"] == "host_kill")
        );
    }

    /// 正常情况：CLI 立即回执，会话稍后才转为空闲；停止成功，不强制结束。
    #[tokio::test(start_paused = true)]
    async fn an_acknowledged_interrupt_does_not_kill_the_cli() {
        let mut upper = Upper::standard();
        upper.initialize(json!({})).await;
        assert_eq!(
            upper.call(send_request("client-1", "work")).await["subtype"],
            "success"
        );
        let response = upper.call(json!({"subtype":"interrupt"})).await;
        assert_eq!(response["subtype"], "success", "{response}");
        assert!(response["response"]["forced"].is_null(), "{response}");
        assert!(
            !upper
                .cli_log()
                .iter()
                .any(|frame| frame["type"] == "host_kill" || frame["type"] == "host_stop")
        );
    }
}
