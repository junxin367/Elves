//! Claude Agent SDK（stream-json 控制协议）传输。
//!
//! 取代原先的“daemon 后台任务 + 终端画面解析”：直接用 CLI 官方的结构化输入输出
//! （`--input-format stream-json --output-format stream-json`，与 Claude Agent SDK 是同一条协议）驱动指定 WSL 中的原版
//! Claude Code，审批、问答、计划确认、中断、模型/权限切换和消息回执都来自结构化帧，不再识别终端屏幕。
//!
//! 组成：
//! - `state`：纯状态机，把 CLI 帧折算成上层需要的状态（无 I/O，可用真实帧回放测试）。
//! - `peer`：异步对端，对上层说与旧共享对端相同的帧协议，所以队列、视图和历史投影不需要改动。
//! - `host.py`：WSL 内常驻宿主，持有 CLI，让执行中的任务不受 Harness 退出和安装影响。
//! - `claude_shared.py`（读取器角色）：历史读取器，只按原生 JSONL 规范化历史、网页消息身份、目标与压缩记录。
//!
//! 用户要求：不使用 bare、替代系统提示词或跳过审批参数，保持原生系统提示、MCP、Skills、Hooks 和权限；
//! 通过 WSL 登录 shell 启动，使 Claude 进程继承目标 WSL 用户的时区；账号、额度等官方信息仍只经同一 WSL 的 CLI 获取。

mod peer;
mod state;

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

use super::{Claude, WslConfig};
use peer::PeerSpec;
pub(super) use peer::{Backend, CliSpec, Io};

const HOST_SCRIPT: &[u8] = include_bytes!("host.py");
const READER_SCRIPT: &[u8] = include_bytes!("../claude_shared.py");

/// 空闲多久后回收 CLI 进程。执行中、等待审批和有后台任务的会话不会被回收；
/// 回收后下次发送用 `--resume` 接上同一个原生会话。
const IDLE_STOP: Duration = Duration::from_secs(10 * 60);

/// 已写入本机数据目录的脚本在 WSL 内的路径。
#[derive(Clone)]
pub(super) struct Scripts {
    pub host: String,
    pub reader: String,
}

/// 把宿主和读取器脚本写到数据目录，并换算成指定 WSL 发行版里的路径。
pub(super) async fn install_scripts(
    directory: &Path,
    config: &WslConfig,
) -> anyhow::Result<Scripts> {
    let host = directory.join("claude-sdk-host.py");
    let reader = directory.join("claude-shared.py");
    crate::settings::atomic_write(&host, HOST_SCRIPT)?;
    crate::settings::atomic_write(&reader, READER_SCRIPT)?;
    Ok(Scripts {
        host: Claude::linux_cwd(
            config,
            host.to_str().context("Claude 宿主脚本路径编码无效")?,
        )
        .await?,
        reader: Claude::linux_cwd(
            config,
            reader.to_str().context("Claude 连接端路径编码无效")?,
        )
        .await?,
    })
}

pub(super) struct WslBackend {
    pub config: WslConfig,
    pub linux_cwd: String,
    pub scripts: Scripts,
    /// 仅对尚未创建的新会话生效的自动压缩窗口（按实例设置）。
    pub auto_compact: Option<String>,
}

impl WslBackend {
    fn command(&self, script: &str) -> anyhow::Result<Command> {
        let mut command = Claude::wsl(&self.config)?;
        command.args(["--cd", &self.linux_cwd]);
        Claude::wsl_login_exec(&mut command, "python3");
        command.args(["-u", script]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }

    async fn cli_arguments(&self, spec: &CliSpec) -> anyhow::Result<Vec<String>> {
        // 用户要求保持原生系统提示、MCP、Skills、Hooks 和权限；不传 bare、替代提示或跳过审批参数。
        let mut argv: Vec<String> = [
            self.config.executable.as_str(),
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ]
        .map(str::to_owned)
        .to_vec();
        if !spec.resumed
            && let Some(window) = &self.auto_compact
        {
            // 用户要求自动压缩窗口按实例设置；只给后续新建会话传原生 CLI 参数。
            argv.extend(["--autocompact".to_owned(), window.clone()]);
        }
        if spec.resumed {
            argv.extend(["--resume".to_owned(), spec.native_id.clone()]);
        } else {
            argv.extend(["--session-id".to_owned(), spec.native_id.clone()]);
        }
        if let Some(mode) = spec.permission_mode.as_deref() {
            let mode = super::supported_permission_mode(&self.config, mode).await?;
            argv.extend(["--permission-mode".to_owned(), mode.clone()]);
            if mode == "bypassPermissions" {
                // 只有用户为此会话明确选择完全访问时，才允许原生 CLI 提供绕过审批能力。
                argv.push("--allow-dangerously-skip-permissions".to_owned());
            }
        }
        Ok(argv)
    }
}

#[async_trait]
impl Backend for WslBackend {
    async fn spawn_cli(&self, spec: &CliSpec) -> anyhow::Result<Io> {
        let argv = self.cli_arguments(spec).await?;
        let mut command = self.command(&self.scripts.host)?;
        command.args(["--session", &spec.native_id]);
        let mut child = command
            .spawn()
            .context("无法启动指定 WSL 中的 Claude 宿主")?;
        let mut stdin = child.stdin.take().context("Claude 宿主 stdin 不可用")?;
        let stdout = child.stdout.take().context("Claude 宿主 stdout 不可用")?;
        let stderr = child.stderr.take().context("Claude 宿主 stderr 不可用")?;
        // 第一行告诉连接端：没有宿主时如何启动 CLI（会话状态事件是对端判断执行状态的依据）。
        let attach = json!({"type":"host_attach","spec":{
            "argv":argv,"cwd":self.linux_cwd,"attach_only":spec.attach_only,
            "env":{"CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS":"1"}}});
        stdin.write_all(format!("{attach}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(Io {
            writer: Box::new(stdin),
            reader: Box::new(stdout),
            stderr: Some(Box::new(stderr)),
            guard: Box::new(child),
        })
    }

    async fn spawn_reader(&self, native_id: &str) -> anyhow::Result<Io> {
        let mut command = self.command(&self.scripts.reader)?;
        command.args(["--cli", &self.config.executable, "--native-id", native_id]);
        let mut child = command
            .spawn()
            .context("无法启动指定 WSL 中的 Claude 历史读取器")?;
        let stdin = child.stdin.take().context("Claude 连接端 stdin 不可用")?;
        let stdout = child.stdout.take().context("Claude 连接端 stdout 不可用")?;
        let stderr = child.stderr.take().context("Claude 连接端 stderr 不可用")?;
        Ok(Io {
            writer: Box::new(stdin),
            reader: Box::new(stdout),
            stderr: Some(Box::new(stderr)),
            guard: Box::new(child),
        })
    }
}

/// 启动一个 SDK 对端，返回与上层相连的读写两端（上层按旧共享连接端的帧协议收发）。
///
/// 两个方向各用一条独立管道：上层丢弃写端时对端读到结束，随即释放 CLI 和读取器连接，
/// 执行中的任务留给宿主继续，空闲则宿主自行收尾。
pub(super) fn connect(
    backend: Arc<dyn Backend>,
    native_id: &str,
    resumed: bool,
) -> (
    impl AsyncWrite + Send + Unpin + 'static,
    impl AsyncRead + Send + Unpin + 'static,
) {
    connect_with(backend, native_id, resumed, IDLE_STOP)
}

pub(super) fn connect_with(
    backend: Arc<dyn Backend>,
    native_id: &str,
    resumed: bool,
    idle_stop: Duration,
) -> (
    impl AsyncWrite + Send + Unpin + 'static,
    impl AsyncRead + Send + Unpin + 'static,
) {
    // 共享快照含整段原生历史投影，单帧可达 64 MiB；管道缓冲只影响吞吐，不限制帧大小。
    let (host_writer, peer_reader) = tokio::io::duplex(1 << 20);
    let (peer_writer, host_reader) = tokio::io::duplex(1 << 20);
    let spec = PeerSpec {
        native_id: native_id.to_owned(),
        resumed,
        idle_stop,
    };
    drop(peer::start(backend, spec, peer_reader, peer_writer));
    (host_writer, host_reader)
}
