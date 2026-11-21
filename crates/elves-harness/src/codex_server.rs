//! Elves 私有 Codex app-server 与 proxy 上的 WebSocket 桥接。
//!
//! 用户要求（2026-11-21）：执行中的 Codex 不随 Harness 退出、重启或安装升级而中断，Harness 再次启动后
//! 自动接上。实现方式经用户确认：Elves 用当前选中的 Codex CLI 启动自己的 app-server，监听只属于本机
//! 用户的私有 unix 套接字，进程脱离 Harness 存活。不复用 Codex 托管 daemon（`codex app-server daemon`）：
//! 它是 Codex 终端版共享的服务，版本自行更新且 CLI 注明更新可能中断执行中的任务，也拿不到 Elves 注入的
//! 工具环境。
//!
//! 控制套接字讲 WebSocket（HTTP Upgrade 后用文本帧收发 JSON-RPC），`codex app-server proxy` 只原样转发
//! 字节。此前 Harness 直接按行写 JSON，握手永远不成立，每次都退回 stdio。这里在 proxy 的 stdio 上完成
//! WebSocket 握手，再把文本帧转换成 Runtime 一直使用的按行 JSON。
//!
//! 服务只在确认空闲时停止（见 CodexHarness::release 与 prepare_controller_shutdown）；身份按 manifest
//! 里的 PID + 进程创建时间 + 可执行文件核对，不会结束复用了同一 PID 的其他进程。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data as OpData, OpCode};

use crate::service_transport::{self, WriteFrame};

const SERVER_DIR: &str = "codex-app-server";
const SOCKET_FILE: &str = "app-server.sock";
/// 启动后等待套接字可握手的上限；覆盖首次加载插件、技能等较慢的冷启动。
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// 单次 proxy 握手 + initialize 的上限。复用已运行的服务时超过即视为无响应：本次退回 stdio，
/// 不结束该服务（它可能正在执行）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 退出时核对未连上的私有服务的总上限；超时按状态未知保留服务，不拖过 Harness 的关停预算。
const INSPECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Codex 服务端在握手响应里声明单条未分片消息上限 16 MiB（x-codex-websocket-max-unfragmented-message-bytes）；
/// 超过 8 MiB 的请求（例如带大图片的 turn/start）按字符边界分片发送，留出一倍余量。
const FRAGMENT_BYTES: usize = 8 * 1024 * 1024;
/// Runtime 与桥接任务之间的内存管道容量，只影响背压，不限制单条消息大小。
const BRIDGE_BUFFER: usize = 1024 * 1024;
/// Windows AF_UNIX 的 sun_path 上限为 108 字节（含结尾 NUL）；留余量，过长时直接退回 stdio。
const MAX_SOCKET_PATH_BYTES: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrivateServer {
    /// 每次启动生成的服务代次；跨 Harness 保留的后台终端记录只在同一代次内有效。
    pub generation: String,
    pub process_id: u32,
    /// 进程创建时间（FILETIME，100ns 单位），与 PID 一起核对身份。
    pub process_created: u64,
    pub socket: PathBuf,
    pub cli_path: PathBuf,
    pub cli_version: String,
    pub codex_home: PathBuf,
    pub started_at_ms: u64,
}

pub(crate) struct ProxyConnection {
    pub child: Child,
    pub stdin: mpsc::Sender<WriteFrame>,
    pub stdout: BufReader<Box<dyn AsyncRead + Send + Sync + Unpin>>,
}

pub(crate) fn supported() -> bool {
    cfg!(windows)
}

fn start_lock() -> &'static Mutex<()> {
    // 同一进程内可能有多处同时首次连接（例如引擎重连与读取并发）；串行化启动，避免为同一 Codex Home
    // 拉起两个服务。会话命名固定走 stdio，不经过这里。
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn server_root() -> PathBuf {
    crate::paths::default_app_state_dir().join(SERVER_DIR)
}

fn normalized_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    text.trim_start_matches(r"\\?\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn server_key(codex_home: &Path) -> String {
    Sha256::digest(normalized_path(codex_home).as_bytes())
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn manifest_path(codex_home: &Path) -> PathBuf {
    server_root().join(format!("{}.json", server_key(codex_home)))
}

fn socket_path(codex_home: &Path) -> PathBuf {
    // Codex 要求套接字所在目录只属于当前用户：目录不存在时由 Codex 自己以私有权限创建，
    // 预先用普通权限建好的目录会被拒绝（"socket directory is not private"）。所以这里只建上一级。
    server_root().join(server_key(codex_home)).join(SOCKET_FILE)
}

fn stderr_path(codex_home: &Path) -> PathBuf {
    server_root().join(format!("{}.stderr.log", server_key(codex_home)))
}

fn same_file(left: &Path, right: &Path) -> bool {
    normalized_path(left) == normalized_path(right)
}

/// 读取 manifest 并核对进程仍在运行、创建时间一致、可执行文件仍是记录的 CLI。
pub(crate) fn running(codex_home: &Path) -> Option<PrivateServer> {
    running_from(&manifest_path(codex_home), codex_home)
}

/// 按当前设置的 Codex Home 查找仍在运行的私有服务（例如上一个 Harness 退出时留下继续执行的服务）。
pub(crate) fn running_for_settings() -> Option<PrivateServer> {
    if !supported() {
        return None;
    }
    let settings = crate::settings::SettingsStore::default()
        .load()
        .unwrap_or_default();
    running(&crate::codex_home::codex_home_dir_for_settings(&settings))
}

fn running_from(manifest: &Path, codex_home: &Path) -> Option<PrivateServer> {
    let bytes = std::fs::read(manifest).ok()?;
    let server: PrivateServer = serde_json::from_slice(&bytes).ok()?;
    let (created, image) = process_identity(server.process_id)?;
    (created == server.process_created
        && same_file(&image, &server.cli_path)
        && same_file(&server.codex_home, codex_home))
    .then_some(server)
}

/// 返回可用的私有服务：同一 CLI 的运行中服务直接复用；CLI 已更新时，只有服务里没有任何已加载会话
/// 才重启换新，否则继续沿用旧版本（可能有任务在执行或后台终端在运行），等下次空闲停止后再换。
pub(crate) async fn ensure(
    cli_path: &Path,
    cli_version: &str,
    codex_home: &Path,
    server_command: &(dyn Fn(&Path) -> Command + Send + Sync),
    proxy_command: &(dyn Fn(&Path) -> Command + Send + Sync),
) -> anyhow::Result<PrivateServer> {
    let _serial = start_lock().lock().await;
    if let Some(server) = running(codex_home) {
        let loaded = probe(proxy_command(&server.socket))
            .await
            .context("Elves 私有 Codex 服务仍在运行但无响应，本次不结束它")?;
        let drifted = !same_file(&server.cli_path, cli_path) || server.cli_version != cli_version;
        if !drifted {
            return Ok(server);
        }
        if !loaded.is_empty() {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_server.version_drift_deferred",
                json!({"processId":server.process_id,"running":server.cli_version,
                    "selected":cli_version,"loadedThreads":loaded.len()}),
            );
            return Ok(server);
        }
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "remote_access.codex_server.version_drift_restart",
            json!({"processId":server.process_id,"running":server.cli_version,"selected":cli_version}),
        );
        stop(&server)?;
    } else {
        let _ = std::fs::remove_file(manifest_path(codex_home));
    }
    start(
        cli_path,
        cli_version,
        codex_home,
        server_command,
        proxy_command,
    )
    .await
}

async fn start(
    cli_path: &Path,
    cli_version: &str,
    codex_home: &Path,
    server_command: &(dyn Fn(&Path) -> Command + Send + Sync),
    proxy_command: &(dyn Fn(&Path) -> Command + Send + Sync),
) -> anyhow::Result<PrivateServer> {
    std::fs::create_dir_all(server_root()).context("无法创建 Elves 私有 Codex 服务目录")?;
    let socket = socket_path(codex_home);
    anyhow::ensure!(
        socket.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
        "Elves 私有 Codex 套接字路径过长：{}",
        socket.display()
    );
    let mut last_error = None;
    for attempt in 0..2 {
        match start_once(
            cli_path,
            cli_version,
            codex_home,
            &socket,
            server_command,
            proxy_command,
        )
        .await
        {
            Ok(server) => return Ok(server),
            Err(error) => {
                let private_dir_rejected = format!("{error:#}").contains("not private");
                last_error = Some(error);
                // 套接字目录若不是 Codex 自己创建的（权限不私有），移开后让 Codex 重新创建一次。
                if attempt == 0
                    && private_dir_rejected
                    && let Some(directory) = socket.parent()
                {
                    let moved = directory.with_extension(format!("stale-{}", crate::now_ms()));
                    if std::fs::rename(directory, &moved).is_ok() {
                        continue;
                    }
                }
                break;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Elves 私有 Codex 服务启动失败")))
}

async fn start_once(
    cli_path: &Path,
    cli_version: &str,
    codex_home: &Path,
    socket: &Path,
    server_command: &(dyn Fn(&Path) -> Command + Send + Sync),
    proxy_command: &(dyn Fn(&Path) -> Command + Send + Sync),
) -> anyhow::Result<PrivateServer> {
    let stderr = std::fs::File::create(stderr_path(codex_home))
        .context("无法创建 Elves 私有 Codex 服务日志")?;
    let mut command = server_command(socket);
    // 服务要比 Harness 活得久：不随句柄释放结束，不接管道，并脱离 Harness 所在的作业对象。
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .kill_on_drop(false);
    #[cfg(windows)]
    command.creation_flags(
        crate::windows_integration::CREATE_NO_WINDOW
            | crate::windows_integration::CREATE_BREAKAWAY_FROM_JOB,
    );
    let mut child = match command.spawn() {
        Ok(child) => child,
        #[cfg(windows)]
        Err(error) if error.raw_os_error() == Some(5) => {
            // 所在作业不允许脱离时退回普通子进程：服务仍可用，但会随该作业一起结束。
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_server.breakaway_denied",
                json!({"error":error.to_string()}),
            );
            command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
            command.spawn().context("启动 Elves 私有 Codex 服务失败")?
        }
        Err(error) => return Err(error).context("启动 Elves 私有 Codex 服务失败"),
    };
    let process_id = child.id().context("无法读取 Elves 私有 Codex 服务进程号")?;
    let deadline = Instant::now() + START_TIMEOUT;
    let ready = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Err(anyhow::anyhow!(
                "Elves 私有 Codex 服务启动后退出（{status}）：{}",
                stderr_tail(codex_home)
            ));
        }
        // 套接字文件可能是上次残留的；以 proxy 握手成功作为就绪，而不是文件存在。
        if socket.exists() && probe(proxy_command(socket)).await.is_ok() {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(anyhow::anyhow!(
                "Elves 私有 Codex 服务未在 {} 秒内就绪：{}",
                START_TIMEOUT.as_secs(),
                stderr_tail(codex_home)
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let identity = ready.and_then(|()| {
        process_identity(process_id).context("无法核对 Elves 私有 Codex 服务进程身份")
    });
    let (process_created, _) = match identity {
        Ok(identity) => identity,
        Err(error) => {
            // 还没有任何客户端使用这个进程，失败时直接回收，避免留下无人管理的服务。
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Err(error);
        }
    };
    let server = PrivateServer {
        generation: uuid::Uuid::new_v4().to_string(),
        process_id,
        process_created,
        socket: socket.to_path_buf(),
        cli_path: cli_path.to_path_buf(),
        cli_version: cli_version.to_owned(),
        codex_home: codex_home.to_path_buf(),
        started_at_ms: crate::now_ms(),
    };
    if let Err(error) =
        crate::settings::atomic_write(&manifest_path(codex_home), &serde_json::to_vec(&server)?)
    {
        let _ = child.start_kill();
        return Err(error).context("无法保存 Elves 私有 Codex 服务记录");
    }
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "remote_access.codex_server.started",
        json!({"processId":process_id,"generation":server.generation,
            "cliVersion":cli_version,"socket":server.socket}),
    );
    // 释放句柄不结束进程（kill_on_drop=false）；之后由 manifest 管理生命周期。
    drop(child);
    Ok(server)
}

/// 只结束 manifest 记录的同一进程（PID + 创建时间）及其子进程树；调用方必须已确认服务空闲。
pub(crate) fn stop(server: &PrivateServer) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let descendants = descendant_process_ids(server.process_id);
        if process_identity(server.process_id)
            .is_some_and(|(created, _)| created == server.process_created)
        {
            anyhow::ensure!(
                terminate_same_process(server.process_id, server.process_created),
                "无法停止 Elves 私有 Codex 服务（进程 {}）",
                server.process_id
            );
            for id in descendants {
                let _ = elves_core::windows_integration::terminate_process(id);
            }
        }
    }
    let manifest = manifest_path(&server.codex_home);
    if std::fs::read(&manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PrivateServer>(&bytes).ok())
        .is_some_and(|saved| saved.generation == server.generation)
    {
        let _ = std::fs::remove_file(manifest);
    }
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "remote_access.codex_server.stopped",
        json!({"processId":server.process_id,"generation":server.generation}),
    );
    Ok(())
}

/// 记录的私有服务进程是否仍在运行（PID + 创建时间一致）。
pub(crate) fn alive(server: &PrivateServer) -> bool {
    process_identity(server.process_id)
        .is_some_and(|(created, _)| created == server.process_created)
}

/// 用服务自己记录的 CLI 连接它；只在没有当前 Codex 候选可用的场合（例如退出时核对）使用。
fn proxy_command_for(server: &PrivateServer) -> Command {
    let mut command = Command::new(&server.cli_path);
    command
        .args(["app-server", "proxy", "--sock"])
        .arg(&server.socket)
        .env("CODEX_HOME", &server.codex_home);
    #[cfg(windows)]
    command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
    command
}

/// 用户要求（2026-11-21）：没有执行中的会话时退出要连同 Codex 一起关闭。私有服务当前没有可用连接时
/// （连接断开，或上次留下、本次尚未连上），退出前经 proxy 只读核对：返回每个已加载会话的 thread/read
/// 结果（含 id、status、name）；with_turns 中的会话额外带上 turns，用于核对交接来的后台终端是否已结束。
/// 不 resume、不发送任何写请求，整体最多 INSPECT_TIMEOUT。
pub(crate) async fn read_loaded_threads(
    server: &PrivateServer,
    with_turns: &std::collections::HashSet<String>,
) -> anyhow::Result<Vec<Value>> {
    tokio::time::timeout(INSPECT_TIMEOUT, async {
        let mut connection = connect(proxy_command_for(server), PROBE_TIMEOUT).await?;
        let result = async {
            let mut next_id = 1u64;
            let mut call = async |connection: &mut ProxyConnection, method: &str, params: Value| {
                let id = next_id;
                next_id += 1;
                send_line(
                    &connection.stdin,
                    &json!({"id":id,"method":method,"params":params}),
                )
                .await?;
                read_response(&mut connection.stdout, id).await
            };
            call(
                &mut connection,
                "initialize",
                json!({"clientInfo":{"name":"elves-harness-shutdown-check",
                    "version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
            )
            .await?;
            send_line(
                &connection.stdin,
                &json!({"method":"initialized","params":{}}),
            )
            .await?;
            let mut ids = Vec::new();
            let mut cursors = std::collections::HashSet::new();
            let mut params = json!({"limit":100});
            loop {
                let page = call(&mut connection, "thread/loaded/list", params.clone()).await?;
                for row in page["result"]["data"].as_array().into_iter().flatten() {
                    let id = row
                        .as_str()
                        .or_else(|| row["id"].as_str())
                        .or_else(|| row["threadId"].as_str())
                        .filter(|id| !id.is_empty())
                        .context("Elves 私有 Codex 服务返回了无效会话 ID")?;
                    if !ids.iter().any(|known| known == id) {
                        ids.push(id.to_owned());
                    }
                }
                let Some(cursor) = page["result"]["nextCursor"]
                    .as_str()
                    .filter(|cursor| !cursor.is_empty())
                else {
                    break;
                };
                anyhow::ensure!(
                    cursors.insert(cursor.to_owned()),
                    "Elves 私有 Codex 服务分页游标重复"
                );
                params["cursor"] = json!(cursor);
            }
            let mut threads = Vec::new();
            for id in ids {
                let response = call(
                    &mut connection,
                    "thread/read",
                    json!({"threadId":id,"includeTurns":with_turns.contains(&id)}),
                )
                .await?;
                let mut thread = response["result"]["thread"].clone();
                anyhow::ensure!(thread.is_object(), "Elves 私有 Codex 服务返回的会话无效");
                thread["id"] = json!(id);
                threads.push(thread);
            }
            anyhow::Ok(threads)
        }
        .await;
        let _ = connection.child.start_kill();
        result
    })
    .await
    .context("核对 Elves 私有 Codex 服务超时")?
}

/// 经 proxy 完成握手、initialize 并读取已加载会话；只读，用于就绪检查与版本漂移判断。
async fn probe(command: Command) -> anyhow::Result<Vec<String>> {
    tokio::time::timeout(PROBE_TIMEOUT, async move {
        let mut connection = connect(command, PROBE_TIMEOUT).await?;
        let result = async {
            let initialize = json!({"id":1,"method":"initialize","params":{
                "clientInfo":{"name":"elves-harness-probe","version":env!("CARGO_PKG_VERSION")},
                "capabilities":{"experimentalApi":true}}});
            send_line(&connection.stdin, &initialize).await?;
            read_response(&mut connection.stdout, 1).await?;
            send_line(
                &connection.stdin,
                &json!({"method":"initialized","params":{}}),
            )
            .await?;
            let list = json!({"id":2,"method":"thread/loaded/list","params":{"limit":100}});
            send_line(&connection.stdin, &list).await?;
            let page = read_response(&mut connection.stdout, 2).await?;
            let mut loaded: Vec<String> = page["result"]["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect();
            if page["result"]["nextCursor"]
                .as_str()
                .is_some_and(|cursor| !cursor.is_empty())
            {
                loaded.push(String::new());
            }
            anyhow::Ok(loaded)
        }
        .await;
        let _ = connection.child.start_kill();
        result
    })
    .await
    .context("Elves 私有 Codex 服务握手超时")?
}

async fn send_line(sender: &mpsc::Sender<WriteFrame>, frame: &Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    service_transport::guarded_write(sender, bytes, None, PROBE_TIMEOUT).await
}

async fn read_response(
    reader: &mut BufReader<Box<dyn AsyncRead + Send + Sync + Unpin>>,
    id: u64,
) -> anyhow::Result<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        anyhow::ensure!(
            reader.read_line(&mut line).await? > 0,
            "Elves 私有 Codex 服务连接已关闭"
        );
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if frame["id"] == id && frame.get("method").is_none() {
            if let Some(error) = frame.get("error") {
                bail!("Elves 私有 Codex 服务拒绝请求：{error}");
            }
            return Ok(frame);
        }
    }
}

/// 启动 `codex app-server proxy` 并在它的 stdio 上完成 WebSocket 握手，返回按行 JSON 的读写端。
pub(crate) async fn connect(
    mut command: Command,
    write_timeout: Duration,
) -> anyhow::Result<ProxyConnection> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // proxy 只属于当前 Harness 连接；Harness 退出时随之结束，私有服务本身不受影响。
        .kill_on_drop(true);
    let mut child = command.spawn().context("启动 Codex proxy 失败")?;
    let stdin = child.stdin.take().context("Codex proxy 标准输入不可用")?;
    let stdout = child.stdout.take().context("Codex proxy 标准输出不可用")?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    let _ = crate::diagnostic_log::append_diagnostic_log(
                        "remote_access.codex_server.proxy_stderr",
                        json!({"message":line.chars().take(500).collect::<String>()}),
                    );
                }
            }
        });
    }
    let config = WebSocketConfig::default()
        .max_message_size(None)
        .max_frame_size(None);
    let handshake = tokio_tungstenite::client_async_with_config(
        "ws://localhost/",
        tokio::io::join(stdout, stdin),
        Some(config),
    );
    let (socket, _) = tokio::time::timeout(PROBE_TIMEOUT, handshake)
        .await
        .context("Codex 私有服务 WebSocket 握手超时")?
        .context("Codex 私有服务 WebSocket 握手失败")?;
    let (stdin, stdout) = bridge(socket, write_timeout);
    Ok(ProxyConnection {
        child,
        stdin,
        stdout,
    })
}

/// 把 WebSocket 文本消息与 Runtime 的按行 JSON 互转；任一方向结束都关闭管道，Runtime 读到 EOF 后按断线处理。
pub(crate) fn bridge<S>(
    socket: WebSocketStream<S>,
    write_timeout: Duration,
) -> (
    mpsc::Sender<WriteFrame>,
    BufReader<Box<dyn AsyncRead + Send + Sync + Unpin>>,
)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (runtime_side, bridge_side) = tokio::io::duplex(BRIDGE_BUFFER);
    let (runtime_read, runtime_write) = tokio::io::split(runtime_side);
    let (bridge_read, mut bridge_write) = tokio::io::split(bridge_side);
    let (mut sink, mut stream) = socket.split();
    let outgoing = tokio::spawn(async move {
        let mut lines = BufReader::new(bridge_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() && send_text(&mut sink, line).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    tokio::spawn(async move {
        while let Some(message) = stream.next().await {
            let text = match message {
                Ok(Message::Text(text)) => text.as_str().to_owned(),
                Ok(Message::Binary(bytes)) => match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => text,
                    Err(_) => continue,
                },
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            // JSON 字符串内的换行必然已转义；裸换行只可能是记号间空白，替换后仍是同一条合法 JSON。
            let line = if text.contains('\n') {
                text.replace(['\r', '\n'], " ")
            } else {
                text
            };
            if bridge_write.write_all(line.as_bytes()).await.is_err()
                || bridge_write.write_all(b"\n").await.is_err()
            {
                break;
            }
        }
        let _ = bridge_write.shutdown().await;
        outgoing.abort();
    });
    let (sender, _task) = service_transport::spawn_writer(runtime_write, write_timeout);
    (sender, BufReader::new(Box::new(runtime_read)))
}

async fn send_text<S>(
    sink: &mut SplitSink<WebSocketStream<S>, Message>,
    text: String,
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if text.len() <= FRAGMENT_BYTES {
        return sink.send(Message::Text(text.into())).await;
    }
    let mut rest = text.as_str();
    let mut first = true;
    while !rest.is_empty() {
        let mut end = rest.len().min(FRAGMENT_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, tail) = rest.split_at(end);
        let opcode = if first {
            OpCode::Data(OpData::Text)
        } else {
            OpCode::Data(OpData::Continue)
        };
        sink.feed(Message::Frame(Frame::message(
            chunk.as_bytes().to_vec(),
            opcode,
            tail.is_empty(),
        )))
        .await?;
        first = false;
        rest = tail;
    }
    sink.flush().await
}

fn stderr_tail(codex_home: &Path) -> String {
    let text = std::fs::read(stderr_path(codex_home))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        // 去掉 ANSI 颜色序列，只保留可读文本。
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        plain.push(ch);
    }
    let lines: Vec<&str> = plain
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    if tail.is_empty() {
        "没有输出".into()
    } else {
        tail.chars()
            .rev()
            .take(600)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

#[cfg(windows)]
fn process_identity(process_id: u32) -> Option<(u64, PathBuf)> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let mut code = 0u32;
        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user).is_ok()
            && GetExitCodeProcess(handle, &mut code).is_ok()
            && code == STILL_ACTIVE
            && QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut size,
            )
            .is_ok();
        let _ = CloseHandle(handle);
        ok.then(|| {
            (
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
                PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize])),
            )
        })
    }
}

#[cfg(not(windows))]
fn process_identity(_process_id: u32) -> Option<(u64, PathBuf)> {
    None
}

#[cfg(windows)]
fn terminate_same_process(process_id: u32, created: u64) -> bool {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        TerminateProcess,
    };
    unsafe {
        let Ok(handle) = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
            false,
            process_id,
        ) else {
            return false;
        };
        let mut start = FILETIME::default();
        let mut end = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let matched = GetProcessTimes(handle, &mut start, &mut end, &mut kernel, &mut user).is_ok()
            && ((u64::from(start.dwHighDateTime) << 32) | u64::from(start.dwLowDateTime))
                == created;
        let terminated = matched && TerminateProcess(handle, 0).is_ok();
        let _ = CloseHandle(handle);
        terminated
    }
}

#[cfg(windows)]
fn descendant_process_ids(root: u32) -> Vec<u32> {
    let processes = elves_core::windows_integration::enumerate_processes();
    let mut found = Vec::new();
    let mut parents = std::collections::HashSet::from([root]);
    let mut changed = true;
    while changed {
        changed = false;
        for process in &processes {
            if parents.contains(&process.parent_process_id)
                && process.process_id != root
                && parents.insert(process.process_id)
            {
                found.push(process.process_id);
                changed = true;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    /// 在内存管道两端分别跑 WebSocket 服务端与桥接，核对按行 JSON 与文本帧互转、超大消息分片后完整送达。
    #[tokio::test]
    async fn bridge_converts_lines_and_fragments_large_requests() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let config = WebSocketConfig::default()
                .max_message_size(None)
                .max_frame_size(None);
            let mut socket = tokio_tungstenite::accept_async_with_config(server_io, Some(config))
                .await
                .unwrap();
            let mut received = Vec::new();
            while received.len() < 2 {
                match socket.next().await.unwrap().unwrap() {
                    Message::Text(text) => received.push(text.as_str().to_owned()),
                    _ => continue,
                }
            }
            // 回一条带裸换行的 JSON，确认桥接后仍是一行且可解析。
            socket
                .send(Message::Text(
                    "{\"id\":7,\n\"result\":{\"ok\":true}}".to_owned().into(),
                ))
                .await
                .unwrap();
            received
        });
        let (socket, _) = tokio_tungstenite::client_async("ws://localhost/", client_io)
            .await
            .unwrap();
        let (sender, mut reader) = bridge(socket, Duration::from_secs(5));
        let small = json!({"id":1,"method":"initialize"}).to_string();
        let large = json!({"id":2,"method":"turn/start","params":{
            "blob":"中".repeat(FRAGMENT_BYTES / 3 + 1024)}})
        .to_string();
        assert!(large.len() > FRAGMENT_BYTES);
        for line in [&small, &large] {
            service_transport::guarded_write(
                &sender,
                format!("{line}\n").into_bytes(),
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        }
        let received = server.await.unwrap();
        assert_eq!(received, vec![small, large]);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(!line.trim_end().contains('\n'));
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["id"], 7);
        assert_eq!(frame["result"]["ok"], true);
    }

    #[test]
    fn server_paths_are_stable_per_codex_home_and_fit_unix_socket_limits() {
        let first = server_key(Path::new(r"C:\Users\me\.codex"));
        assert_eq!(first, server_key(Path::new(r"c:/users/ME/.codex/")));
        assert_ne!(first, server_key(Path::new(r"C:\Users\me\.codex-work")));
        assert_eq!(first.len(), 12);
        assert!(socket_path(Path::new(r"C:\Users\me\.codex")).ends_with(SOCKET_FILE));
    }

    #[test]
    fn manifest_for_a_dead_or_foreign_process_is_not_reused() {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let server = PrivateServer {
            generation: "g".into(),
            process_id: std::process::id(),
            // 当前测试进程的 PID，但创建时间对不上：必须视为不同进程。
            process_created: 1,
            socket: socket_path(&codex_home),
            cli_path: std::env::current_exe().unwrap(),
            cli_version: "0".into(),
            codex_home: codex_home.clone(),
            started_at_ms: 0,
        };
        let manifest = directory.path().join("server.json");
        std::fs::write(&manifest, serde_json::to_vec(&server).unwrap()).unwrap();
        assert!(running_from(&manifest, &codex_home).is_none());
        // 同一 Codex Home 之外的记录也不能复用。
        let mut foreign = server.clone();
        foreign.codex_home = directory.path().join("other-home");
        std::fs::write(&manifest, serde_json::to_vec(&foreign).unwrap()).unwrap();
        assert!(running_from(&manifest, &codex_home).is_none());
    }
}
