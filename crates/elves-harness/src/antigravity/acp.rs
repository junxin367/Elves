//! Antigravity 官方 CLI 的 ACP（Agent Client Protocol）stdio JSON-RPC 2.0 传输层。
//!
//! 严格遵循 AGENTS.md 约定：所有与 Antigravity 的交互、状态读取、会话管理及能力探测
//! 均只通过官方 `agy acp` 提供的标准 ACP 协议进行，不绕过 CLI 读写内部状态文件或提取凭据。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

use super::super::ApiError;
use crate::service_transport::{WriteFrame, guarded_write, spawn_writer};

const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const STDERR_TAIL_LIMIT: usize = 8192;
const ANTIGRAVITY_AUTH_STDOUT_PREFIX: &str =
    "Open the following link to authenticate the ACP server: ";

/// Antigravity 运行环境配置：支持 Windows 本机（distribution 为空）与 WSL 发行版实例。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct EnvConfig {
    pub distribution: String,
    pub user: String,
    pub executable: String,
}

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            distribution: String::new(),
            user: String::new(),
            executable: String::new(),
        }
    }
}

impl EnvConfig {
    pub(crate) fn is_local(&self) -> bool {
        self.distribution.is_empty()
    }

    pub(crate) fn instance_id(&self) -> String {
        super::super::instances::id("antigravity", &self.distribution, &self.user)
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.executable.is_empty() && !self.executable.contains('\0'),
            "Antigravity 可执行文件路径无效"
        );
        if self.is_local() {
            anyhow::ensure!(
                Path::new(&self.executable).is_absolute(),
                "本机 Antigravity 可执行文件路径必须为绝对路径"
            );
            return Ok(());
        }
        anyhow::ensure!(
            super::super::preferences::valid_distribution_name(&self.distribution)
                && !self.user.is_empty()
                && !self.user.contains('\0')
                && self.executable.starts_with('/'),
            "Antigravity WSL 配置无效"
        );
        Ok(())
    }
}

/// 来自 ACP Agent 的反向请求（如 `session/request_permission`）。
#[derive(Debug, Clone)]
pub(crate) struct AcpIncomingRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// 单条活跃的 `agy acp` JSON-RPC 2.0 进程连接。
pub(crate) struct AcpClient {
    writer: mpsc::Sender<WriteFrame>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, ApiError>>>>>,
    live: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<String>>,
    child: Mutex<Option<Child>>,
    pub(crate) initialize_result: Mutex<Value>,
}

pub(crate) struct AcpChannels {
    pub updates: mpsc::UnboundedReceiver<Value>,
    pub requests: mpsc::UnboundedReceiver<AcpIncomingRequest>,
}

impl AcpClient {
    /// 启动指定本机或 WSL 环境中的 `agy acp` 进程；恢复桌面 App/IDE 历史会话时可传官方 `--app-data-dir` 参数，
    /// 选择“无限制模式”时传递官方 `--dangerously-skip-permissions` 参数。
    pub(crate) async fn spawn(
        config: &EnvConfig,
        cwd: &str,
        linux_cwd: &str,
        app_data_dir: Option<&str>,
        dangerously_skip_permissions: bool,
    ) -> anyhow::Result<(Arc<Self>, AcpChannels)> {
        config.validate()?;
        let verified_data_dir = app_data_dir.filter(|dir| super::local_store::valid_app_data_dir(dir));
        let mut command = if config.is_local() {
            let mut cmd = Command::new(&config.executable);
            cmd.arg("acp");
            if dangerously_skip_permissions {
                cmd.arg("--dangerously-skip-permissions");
            }
            if let Some(dir) = verified_data_dir {
                cmd.args(["--app-data-dir", dir]);
            }
            if !cwd.is_empty() && Path::new(cwd).is_dir() {
                cmd.current_dir(cwd);
            }
            cmd
        } else {
            anyhow::ensure!(cfg!(windows), "WSL Antigravity 实例需要 Windows WSL2");
            let mut cmd = Command::new("wsl.exe");
            cmd.args([
                "--distribution",
                &config.distribution,
                "--user",
                &config.user,
            ]);
            if !linux_cwd.is_empty() {
                cmd.args(["--cd", linux_cwd]);
            }
            // 用户与系统时区及 PATH 须来自目标 WSL 登录环境，避免绕过 /etc/profile。
            cmd.args(["--exec", "/bin/bash", "-lc", "exec \"$@\"", "--"]);
            cmd.arg(&config.executable);
            cmd.arg("acp");
            if dangerously_skip_permissions {
                cmd.arg("--dangerously-skip-permissions");
            }
            if let Some(dir) = verified_data_dir {
                cmd.args(["--app-data-dir", dir]);
            }
            cmd
        };
        #[cfg(windows)]
        command.creation_flags(crate::windows_create_hidden_process());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        Self::spawn_command(command).await
    }

    pub(crate) async fn spawn_command(mut command: Command) -> anyhow::Result<(Arc<Self>, AcpChannels)> {
        let mut child = command.spawn().context("无法启动 Antigravity ACP CLI")?;
        let stdin = child.stdin.take().context("Antigravity ACP stdin 不可用")?;
        let stdout = child.stdout.take().context("Antigravity ACP stdout 不可用")?;
        let stderr = child.stderr.take().context("Antigravity ACP stderr 不可用")?;

        let (writer, _) = spawn_writer(stdin, WRITE_TIMEOUT);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, ApiError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let live = Arc::new(AtomicBool::new(true));
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let (update_tx, update_rx) = mpsc::unbounded_channel();
        let (request_tx, request_rx) = mpsc::unbounded_channel();

        let stderr_sink = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut tail = stderr_sink.lock().await;
                if tail.len() < STDERR_TAIL_LIMIT {
                    tail.push_str(&line);
                    tail.push('\n');
                }
            }
        });

        let read_pending = pending.clone();
        let read_live = live.clone();
        let read_stderr = stderr_tail.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            let reason = loop {
                line.clear();
                let read = (&mut reader)
                    .take((MAX_FRAME_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line)
                    .await;
                let size = match read {
                    Ok(0) => break String::from("Antigravity ACP 进程已退出"),
                    Ok(size) => size,
                    Err(error) => break format!("读取 Antigravity ACP 输出失败：{error}"),
                };
                if size > MAX_FRAME_BYTES {
                    break String::from("Antigravity ACP 输出帧超过 32 MiB 上限");
                }
                let text = String::from_utf8_lossy(&line);
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.starts_with(ANTIGRAVITY_AUTH_STDOUT_PREFIX) {
                    // 未登录环境下 agy acp 收到 authenticate 会打印 OAuth 链接并阻塞等待浏览器回调；
                    // Elves 不在后台探测或普通请求中无限阻塞，立即按 401 返回登录指引。
                    let mut waiters = read_pending.lock().await;
                    for (_, tx) in waiters.drain() {
                        let _ = tx.send(Err(ApiError::new(
                            401,
                            "Antigravity 实例尚未登录，请先在目标环境执行 `agy` 或 `agy acp --login` 完成登录后重试",
                        )
                        .into()));
                    }
                    break String::from("Antigravity 实例尚未登录，已停止等待交互式 OAuth");
                }
                let Ok(frame) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                let has_method = frame.get("method").and_then(Value::as_str).is_some();
                let has_id = frame.get("id").is_some_and(|id| !id.is_null());
                if has_method && has_id {
                    let method = frame["method"].as_str().unwrap_or_default().to_owned();
                    let _ = request_tx.send(AcpIncomingRequest {
                        id: frame["id"].clone(),
                        method,
                        params: frame.get("params").cloned().unwrap_or(Value::Null),
                    });
                } else if has_method {
                    let _ = update_tx.send(frame);
                } else if has_id {
                    if let Some(id) = frame["id"].as_u64()
                        && let Some(tx) = read_pending.lock().await.remove(&id)
                    {
                        if let Some(error) = frame.get("error").filter(|v| !v.is_null()) {
                            let code = error["code"].as_i64().unwrap_or(-32603);
                            let msg = error["message"].as_str().unwrap_or("ACP 请求失败");
                            let _ = tx.send(Err(map_acp_rpc_error(code, msg)));
                        } else {
                            let _ = tx.send(Ok(frame.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                }
            };
            read_live.store(false, Ordering::Release);
            let tail = read_stderr.lock().await.trim().to_owned();
            let detail = if tail.is_empty() {
                reason
            } else {
                format!("{reason}（{}）", tail.chars().take(240).collect::<String>())
            };
            let mut waiters = read_pending.lock().await;
            for (_, tx) in waiters.drain() {
                let _ = tx.send(Err(ApiError::new(503, detail.clone())));
            }
        });

        Ok((
            Arc::new(Self {
                writer,
                next_id: AtomicU64::new(1),
                pending,
                live,
                stderr_tail,
                child: Mutex::new(Some(child)),
                initialize_result: Mutex::new(Value::Null),
            }),
            AcpChannels {
                updates: update_rx,
                requests: request_rx,
            },
        ))
    }

    pub(crate) fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_closed_writer() -> Arc<Self> {
        // 确定性模拟 stdout 仍在线但写入队列已关闭，不依赖 Windows 管道关闭的调度时序。
        let (writer, receiver) = mpsc::channel(1);
        drop(receiver);
        Arc::new(Self {
            writer,
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            live: Arc::new(AtomicBool::new(true)),
            stderr_tail: Arc::new(Mutex::new(String::new())),
            child: Mutex::new(None),
            initialize_result: Mutex::new(Value::Null),
        })
    }

    pub(crate) async fn close(&self) {
        self.live.store(false, Ordering::Release);
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
    }

    async fn write_frame(&self, frame: &Value) -> anyhow::Result<()> {
        let mut bytes = serde_json::to_vec(frame)?;
        bytes.push(b'\n');
        guarded_write(&self.writer, bytes, None, WRITE_TIMEOUT).await
    }

    pub(crate) async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self.write_frame(&frame).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        let reply = match timeout {
            Some(duration) => match tokio::time::timeout(duration, rx).await {
                Ok(received) => received,
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    let tail = self.stderr_tail.lock().await.trim().to_owned();
                    let extra = if tail.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", tail.chars().take(200).collect::<String>())
                    };
                    return Err(ApiError::new(
                        504,
                        format!("Antigravity ACP 请求 {method} 超时{extra}"),
                    )
                    .into());
                }
            },
            None => rx.await,
        };
        match reply {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(api_err)) => Err(api_err.into()),
            Err(_) => Err(ApiError::new(503, format!("Antigravity ACP 连接已中断（{method}）")).into()),
        }
    }

    pub(crate) async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.write_frame(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    pub(crate) async fn respond_result(&self, id: Value, result: Value) -> anyhow::Result<()> {
        self.write_frame(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .await
    }

    pub(crate) async fn respond_error(
        &self,
        id: Value,
        code: i64,
        message: &str,
    ) -> anyhow::Result<()> {
        self.write_frame(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .await
    }

    pub(crate) async fn initialize(&self) -> anyhow::Result<Value> {
        let result = self
            .request_with_timeout(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": false, "writeTextFile": false },
                        "terminal": false,
                        "_meta": { "antigravity": { "tier": true } }
                    },
                    "clientInfo": {
                        "name": "elves",
                        "title": "Elves",
                        "version": crate::version::VERSION
                    }
                }),
                Some(RPC_TIMEOUT),
            )
            .await?;
        *self.initialize_result.lock().await = result.clone();
        Ok(result)
    }

    pub(crate) async fn authenticate(&self, method_id: &str) -> anyhow::Result<Value> {
        self.request_with_timeout(
            "authenticate",
            json!({ "methodId": method_id }),
            Some(RPC_TIMEOUT),
        )
        .await
    }

    pub(crate) async fn session_list(&self) -> anyhow::Result<Vec<Value>> {
        let result = self
            .request_with_timeout("session/list", json!({}), Some(RPC_TIMEOUT))
            .await?;
        Ok(result["sessions"].as_array().cloned().unwrap_or_default())
    }

    pub(crate) async fn session_new(
        &self,
        cwd: &str,
        system_prompt_append: Option<&str>,
    ) -> anyhow::Result<Value> {
        let init = self.initialize_result.lock().await.clone();
        let supports_append = init["agentCapabilities"]["_meta"]["antigravity"]["systemPrompt"]
            ["append"]
            == true;
        let mut params = json!({
            "cwd": cwd,
            "mcpServers": [],
        });
        if let Some(append) = system_prompt_append.filter(|s| !s.trim().is_empty())
        {
            // 历史编辑必须完整保留前文；官方未宣告追加能力时拒绝，不能悄悄创建缺失上下文的会话。
            anyhow::ensure!(supports_append, ApiError::new(
                501, "当前 Antigravity ACP 未提供历史上下文追加能力，无法安全编辑历史消息",
            ));
            params["_meta"] = json!({
                "antigravity": { "systemPrompt": { "append": append } }
            });
        }
        self.request_with_timeout("session/new", params, Some(RPC_TIMEOUT))
            .await
    }

    pub(crate) async fn session_load(&self, session_id: &str, cwd: &str) -> anyhow::Result<Value> {
        self.request_with_timeout(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": cwd,
                "mcpServers": [],
            }),
            Some(RPC_TIMEOUT),
        )
        .await
    }

    pub(crate) async fn session_resume(
        &self,
        session_id: &str,
        cwd: &str,
    ) -> anyhow::Result<Value> {
        self.request_with_timeout(
            "session/resume",
            json!({
                "sessionId": session_id,
                "cwd": cwd,
                "mcpServers": [],
            }),
            Some(RPC_TIMEOUT),
        )
        .await
    }

    pub(crate) async fn session_close(&self, session_id: &str) -> anyhow::Result<Value> {
        self.request_with_timeout(
            "session/close",
            json!({ "sessionId": session_id }),
            Some(Duration::from_secs(15)),
        )
        .await
    }

    pub(crate) async fn session_set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> anyhow::Result<Value> {
        self.request_with_timeout(
            "session/set_config_option",
            json!({
                "sessionId": session_id,
                "configId": config_id,
                "value": value,
            }),
            Some(RPC_TIMEOUT),
        )
        .await
    }

    pub(crate) async fn session_set_mode(
        &self,
        session_id: &str,
        mode_id: &str,
    ) -> anyhow::Result<Value> {
        self.request_with_timeout(
            "session/set_mode",
            json!({
                "sessionId": session_id,
                "modeId": mode_id,
            }),
            Some(RPC_TIMEOUT),
        )
        .await
    }

    pub(crate) async fn session_prompt(
        &self,
        session_id: &str,
        prompt: Vec<Value>,
    ) -> anyhow::Result<Value> {
        let result = self.request_with_timeout(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": prompt,
            }),
            None,
        )
        .await;
        // 写入后连接中断不能等同于未发送。保留未知回执，阻止队列自动重发同一消息。
        result.map_err(|failure| {
            if let Some(dispatch) = failure.downcast_ref::<crate::service_transport::DispatchFailure>() {
                let mut mapped = ApiError::new(503, failure.to_string());
                mapped.outcome_unknown = dispatch.dispatched;
                return mapped.into();
            }
            if let Some(api) = failure.downcast_ref::<ApiError>().filter(|e| matches!(e.code, 503 | 504)) {
                let mut mapped = api.clone();
                mapped.outcome_unknown = true;
                return mapped.into();
            }
            failure
        })
    }

    pub(crate) async fn session_cancel(&self, session_id: &str) -> anyhow::Result<()> {
        self.notify("session/cancel", json!({ "sessionId": session_id }))
            .await
    }
}

/// 将 ACP JSON-RPC 错误码转换为面向用户的中文提示。
pub(crate) fn map_acp_rpc_error(code: i64, message: &str) -> ApiError {
    let lower = message.to_ascii_lowercase();
    if code == -32000
        || lower.contains("authentication required")
        || lower.contains("no browser is available here")
    {
        return ApiError::new(
            401,
            "Antigravity 实例尚未登录，请先在对应本机或 WSL 终端运行 `agy` 或 `agy acp --login` 完成 Google 登录。",
        );
    }
    if lower.contains("subscription_required") {
        return ApiError::new(403, "当前 Google 账号需要有效的 Antigravity 订阅才能使用。");
    }
    if lower.contains("trajectory not found") {
        return ApiError::new(
            404,
            format!("未在当前数据目录找到该 Antigravity 会话轨迹（可能已在原生客户端删除或尚未落盘）：{message}"),
        );
    }
    if code == -32601 {
        return ApiError::new(
            501,
            format!("当前 Antigravity ACP 协议暂不支持该操作（{message}）"),
        );
    }
    if code == -32602 {
        return ApiError::new(400, format!("Antigravity ACP 参数无效：{message}"));
    }
    ApiError::new(503, format!("Antigravity ACP 错误：{message}"))
}
