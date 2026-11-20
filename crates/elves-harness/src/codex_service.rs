use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::service_transport::{self, WriteFrame};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(20);
const PROXY_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_SOCKET_DIR: &str = "app-server-control";
const DAEMON_SOCKET_FILE: &str = "app-server-control.sock";
const DAEMON_MANIFEST_FILE: &str = "elves-codex-daemon.json";

pub trait ServiceDispatchPermit: Send {
    fn commit(
        self: Box<Self>,
        dispatch: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
    ) -> anyhow::Result<()>;
}

#[async_trait]
pub trait ServiceDispatchGuard: Send + Sync {
    async fn prepare_dispatch(&self) -> anyhow::Result<Box<dyn ServiceDispatchPermit>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceTransport {
    DaemonProxy,
    Stdio,
}

impl ServiceTransport {
    pub(crate) fn execution_label(self) -> &'static str {
        match self {
            Self::DaemonProxy => "app-server-daemon-proxy",
            Self::Stdio => "app-server-stdio",
        }
    }

    pub(crate) fn supports_handoff(self) -> bool {
        matches!(self, Self::DaemonProxy)
    }
}

pub(crate) struct RunningService {
    pub(crate) child: Child,
    // 占用检查绑定实际启动的实例目录，不能借用后来切换的设置或其他 Codex Home。
    pub(crate) codex_home: Option<PathBuf>,
    pub(crate) transport: ServiceTransport,
    pub(crate) stdin: mpsc::Sender<WriteFrame>,
    pub(crate) stdout: BufReader<ChildStdout>,
}

#[cfg(test)]
impl RunningService {
    pub(crate) fn for_test(child: Child) -> anyhow::Result<Self> {
        Self::for_test_with_transport(child, ServiceTransport::Stdio)
    }

    pub(crate) fn for_test_with_transport(
        mut child: Child,
        transport: ServiceTransport,
    ) -> anyhow::Result<Self> {
        let (stdin, _) = service_transport::spawn_writer(
            child.stdin.take().context("missing stdin")?,
            REQUEST_TIMEOUT,
        );
        let stdout = BufReader::new(child.stdout.take().context("missing stdout")?);
        Ok(Self {
            child,
            codex_home: None,
            transport,
            stdin,
            stdout,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    path: PathBuf,
    shell: bool,
    version: Option<crate::cli_wrapper::CodexCliVersion>,
    ordinal: usize,
}

pub(crate) async fn spawn_service() -> anyhow::Result<RunningService> {
    let settings = crate::settings::SettingsStore::default()
        .load()
        .unwrap_or_default();
    let codex_home = crate::codex_home::codex_home_dir_for_settings(&settings);
    let candidates = discover_candidates().await;
    let mut errors = Vec::new();
    for candidate in candidates {
        match spawn_candidate(&candidate, &codex_home).await {
            Ok(service) => {
                let _ = crate::diagnostic_log::append_diagnostic_log(
                    "remote_access.codex_service.started",
                    json!({
                        "executable": candidate.path.to_string_lossy(),
                        "shell": candidate.shell,
                        "processId": service.child.id(),
                        "codexHome": codex_home.to_string_lossy(),
                        "transport": service.transport.execution_label(),
                        "hidden": true
                    }),
                );
                return Ok(service);
            }
            Err(error) => errors.push(format!("{}: {error}", candidate.path.display())),
        }
    }
    let message = if errors.is_empty() {
        "未找到 Codex CLI 可执行文件".to_string()
    } else {
        format!("无法启动 Codex CLI app-server：{}", errors.join("；"))
    };
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "remote_access.codex_service.unavailable",
        json!({ "error": truncate_error(&message) }),
    );
    bail!(message)
}

async fn spawn_candidate(
    candidate: &Candidate,
    codex_home: &Path,
) -> anyhow::Result<RunningService> {
    let socket = daemon_socket_path(codex_home);
    let socket_was_present = socket.is_file();
    let (mut command, transport) = match ensure_daemon(candidate, codex_home).await {
        Ok(socket) if proxy_is_usable(candidate, codex_home, &socket).await => {
            (proxy_command(candidate, &socket), ServiceTransport::DaemonProxy)
        }
        Ok(socket) => {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_service.proxy_unavailable",
                json!({
                    "executable": candidate.path.to_string_lossy(),
                    "socket": socket,
                    "reason": "initialize_timeout_or_invalid_response",
                }),
            );
            // 用户要求安装升级不能因代理握手失败而误杀执行中的 Codex；已有 daemon
            // 套接字无法完成 proxy 握手时必须硬失败，不能再启动受 Harness 生命周期控制的 stdio。
            bail!("Codex 持久服务代理握手失败，拒绝降级到 stdio")
        }
        Err(error) => {
            if socket_was_present || socket.is_file() {
                let _ = crate::diagnostic_log::append_diagnostic_log(
                    "remote_access.codex_service.daemon_unavailable",
                    json!({
                        "executable": candidate.path.to_string_lossy(),
                        "socket": socket,
                        "reason": error.to_string(),
                        "fallback": "blocked",
                    }),
                );
                return Err(error);
            }
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_service.daemon_unavailable",
                json!({
                    "executable": candidate.path.to_string_lossy(),
                    "fallback": "stdio",
                    "reason": error.to_string(),
                }),
            );
            // 只有没有任何 daemon 套接字、无法启动持久服务时才保留 stdio 兜底；
            // Runtime 会把它标为不可 handoff，安装升级不得把该连接报告为安全交接。
            (candidate_command(candidate), ServiceTransport::Stdio)
        }
    };
    // 用户要求服务模式独立于 Codex App；为 app-server 及其命令子进程注入 Elves 自带工具，
    // 只修改子进程环境，不写入用户或系统 PATH。
    let tool_runtime = crate::tool_runtime::apply_to_command(&mut command);
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "remote_access.codex_service.tool_runtime",
        json!({"runtime":tool_runtime.diagnostic_value()}),
    );
    // 用户要求 Node REPL 与 Codex App 使用同一 CLI 入口；node_repl 通过此环境变量定位当前 Codex。
    command.env("CODEX_CLI_PATH", &candidate.path);

    let working_directory = std::env::current_dir().context("读取 Elves 工作目录失败")?;
    // 服务模式只指定 Codex 数据目录；保留真实 HOME/USERPROFILE，避免 Cargo、Git 等工具读错用户配置。
    command
        .current_dir(working_directory)
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Harness 只拥有 proxy；原生 app-server daemon 独立存活，升级时不能随 proxy 一起终止。
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .with_context(|| format!("启动 {} 失败", candidate.path.display()))?;
    let stdin = child
        .stdin
        .take()
        .context("Codex CLI service 标准输入不可用")?;
    let (stdin, _writer_task) = service_transport::spawn_writer(stdin, REQUEST_TIMEOUT);
    let stdout = child
        .stdout
        .take()
        .context("Codex CLI service 标准输出不可用")?;
    if let Some(stderr) = child.stderr.take() {
        let executable = candidate.path.to_string_lossy().to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = crate::diagnostic_log::append_diagnostic_log(
                    "remote_access.codex_service.stderr",
                    json!({
                        "executable": executable,
                        "message": truncate_error(&line)
                    }),
                );
            }
        });
    }
    Ok(RunningService {
        child,
        codex_home: Some(codex_home.to_path_buf()),
        transport,
        stdin,
        stdout: BufReader::new(stdout),
    })
}

async fn proxy_is_usable(candidate: &Candidate, codex_home: &Path, socket: &Path) -> bool {
    let mut command = proxy_command(candidate, socket);
    command
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return false;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return false;
    };
    let mut stdout = BufReader::new(stdout).lines();
    let mut request = match serde_json::to_vec(&json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "elves-harness-probe",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {"experimentalApi": true}
        }
    })) {
        Ok(request) => request,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return false;
        }
    };
    request.push(b'\n');
    let write_ok = matches!(
        tokio::time::timeout(PROXY_PROBE_TIMEOUT, stdin.write_all(&request)).await,
        Ok(Ok(()))
    ) && matches!(
        tokio::time::timeout(PROXY_PROBE_TIMEOUT, stdin.flush()).await,
        Ok(Ok(()))
    );
    let usable = if write_ok {
        match tokio::time::timeout(PROXY_PROBE_TIMEOUT, stdout.next_line()).await {
            Ok(Ok(Some(line))) => serde_json::from_str::<Value>(&line)
                .ok()
                .is_some_and(|value| value["id"] == 1 && value["result"].is_object()),
            _ => false,
        }
    } else {
        false
    };
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    usable
}

fn same_path(left: &str, right: &Path) -> bool {
    let normalize = |value: &str| {
        value
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(&right.to_string_lossy())
}

fn daemon_version_confirms_socket(value: &Value, socket: &Path) -> bool {
    value["status"] == "running"
        && value["socketPath"]
            .as_str()
            .is_some_and(|path| same_path(path, socket))
}

async fn probe_daemon_version(candidate: &Candidate, codex_home: &Path) -> Option<Value> {
    let mut command = command_with_args(candidate, ["app-server", "daemon", "version"]);
    command
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(DAEMON_START_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn write_daemon_manifest(
    manifest_path: &Path,
    candidate: &Candidate,
    socket: &Path,
    daemon_version: Option<&Value>,
) -> anyhow::Result<()> {
    let expected_version = candidate
        .version
        .as_ref()
        .map(|version| version.raw.clone())
        .context("无法确认 Codex CLI 版本，拒绝接管持久服务")?;
    let expected_path = candidate.path.to_string_lossy().to_string();
    crate::settings::atomic_write(
        manifest_path,
        &serde_json::to_vec(&json!({
            "cliPath": expected_path,
            "cliVersion": expected_version,
            "socket": socket,
            "managedCodexPath": daemon_version.and_then(|value| value["managedCodexPath"].as_str()),
            "managedCodexVersion": daemon_version.and_then(|value| value["managedCodexVersion"].as_str()),
            "appServerVersion": daemon_version.and_then(|value| value["appServerVersion"].as_str()),
            "createdAtMs": crate::now_ms()
        }))?,
    )?;
    Ok(())
}

fn daemon_socket_path(codex_home: &Path) -> PathBuf {
    codex_home.join(DAEMON_SOCKET_DIR).join(DAEMON_SOCKET_FILE)
}

async fn ensure_daemon(candidate: &Candidate, codex_home: &Path) -> anyhow::Result<PathBuf> {
    let socket = daemon_socket_path(codex_home);
    let manifest_path = codex_home.join(DAEMON_MANIFEST_FILE);
    let expected_version = candidate
        .version
        .as_ref()
        .map(|version| version.raw.clone())
        .context("无法确认 Codex CLI 版本，拒绝接管持久服务")?;
    let expected_path = candidate.path.to_string_lossy().to_string();
    if socket.is_file() {
        // 清单完整且匹配时直接复用；如果清单明确属于其他 CLI，不能用一次
        // version 探测覆盖它。只有缺失或损坏的清单才允许进入恢复分支。
        let manifest = match std::fs::read(&manifest_path) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("无法读取 Codex 持久服务代次：{}", manifest_path.display())
                });
            }
        };
        if let Some(manifest) = manifest {
            let has_identity = manifest["cliPath"].is_string()
                && manifest["cliVersion"].is_string();
            if has_identity {
                anyhow::ensure!(
                    manifest["cliPath"] == expected_path
                        && manifest["cliVersion"] == expected_version,
                    "Codex 持久服务版本或路径不匹配，拒绝复用旧服务"
                );
                return Ok(socket);
            }
        }
        // 安装升级可能只留下原生守护进程和套接字，清单尚未写入或内容损坏；
        // 先用 CLI 只读核对，确认同一套接字仍由运行中的 app-server 提供后再补写清单。
        if let Some(daemon_version) = probe_daemon_version(candidate, codex_home).await
            && daemon_version_confirms_socket(&daemon_version, &socket)
        {
            write_daemon_manifest(&manifest_path, candidate, &socket, Some(&daemon_version))?;
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "remote_access.codex_service.daemon_manifest_recovered",
                json!({
                    "executable": candidate.path.to_string_lossy(),
                    "socket": socket,
                    "managedCodexPath": daemon_version["managedCodexPath"],
                    "managedCodexVersion": daemon_version["managedCodexVersion"],
                    "appServerVersion": daemon_version["appServerVersion"],
                }),
            );
            return Ok(socket);
        }
        bail!(
            "无法读取 Codex 持久服务代次：{}；运行中的套接字未通过 CLI 状态核对",
            manifest_path.display()
        );
    }
    let mut command = command_with_args(candidate, ["app-server", "daemon", "start"]);
    #[cfg(windows)]
    command.creation_flags(
        crate::windows_integration::CREATE_NO_WINDOW
            | crate::windows_integration::CREATE_BREAKAWAY_FROM_JOB,
    );
    command
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(DAEMON_START_TIMEOUT, command.output())
        .await
        .context("启动 Codex 持久服务超时")?
        .context("启动 Codex 持久服务失败")?;
    if !output.status.success() && !socket.is_file() {
        bail!(
            "Codex 持久服务启动失败（{}）：{}",
            output.status,
            truncate_error(&String::from_utf8_lossy(&output.stderr))
        );
    }
    let deadline = tokio::time::Instant::now() + DAEMON_START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if socket.is_file() {
            write_daemon_manifest(&manifest_path, candidate, &socket, None)?;
            return Ok(socket);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("Codex 持久服务未创建控制套接字：{}", socket.display())
}

fn proxy_command(candidate: &Candidate, socket: &Path) -> Command {
    command_with_args(
        candidate,
        [
            "app-server",
            "proxy",
            "--sock",
            socket.to_string_lossy().as_ref(),
        ],
    )
}

fn command_with_args<const N: usize>(
    candidate: &Candidate,
    args: [&str; N],
) -> Command {
    #[cfg(not(windows))]
    if candidate.shell {
        let command_line = std::iter::once(format!("\"{}\"", candidate.path.display()))
            .chain(
                args.iter()
                    .map(|arg| format!("\"{}\"", arg.replace('"', "\\\""))),
            )
            .collect::<Vec<_>>()
            .join(" ");
        let mut command = Command::new("sh");
        command.args(["-lc", &command_line]);
        return command;
    }

    // Rust invokes .cmd/.bat through cmd.exe with the required quoting. Building
    // a cmd /c string with ordinary args escapes its quotes as literal backslashes.
    let mut command = Command::new(&candidate.path);
    command.args(args);
    #[cfg(windows)]
    command.creation_flags(crate::windows_create_hidden_process());
    command
}

fn candidate_command(candidate: &Candidate) -> Command {
    command_with_args(candidate, ["app-server", "--analytics-default-enabled"])
}

async fn discover_candidates() -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |path: PathBuf, shell: bool| {
        if crate::cli_wrapper::is_desktop_codex_path(&path) {
            return;
        }
        let key = format!("{}:{shell}", path.to_string_lossy().to_ascii_lowercase());
        if seen.insert(key) {
            let ordinal = candidates.len();
            candidates.push(Candidate {
                path,
                shell,
                version: None,
                ordinal,
            });
        }
    };

    for path in crate::cli_wrapper::default_user_runtime_candidates() {
        if path.is_file() {
            add(path, false);
        }
    }
    let mut discovery_command = Command::new(if cfg!(windows) { "where.exe" } else { "sh" });
    discovery_command.args(if cfg!(windows) {
        vec!["codex".to_string()]
    } else {
        vec!["-lc".to_string(), "command -v codex".to_string()]
    });
    #[cfg(windows)]
    discovery_command.creation_flags(crate::windows_create_hidden_process());
    if let Ok(output) = discovery_command.output().await {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let path = PathBuf::from(line.trim());
            if path.as_os_str().is_empty() {
                continue;
            }
            if cfg!(windows) && path.extension().is_none() {
                continue;
            }
            let shell = path.extension().is_none()
                || path.extension().is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("cmd")
                        || extension.eq_ignore_ascii_case("bat")
                        || extension.eq_ignore_ascii_case("sh")
                });
            add(path, shell);
        }
    }
    add(PathBuf::from("codex"), true);
    // 保留主线的 CLI 选择约束：按实际版本优先使用较新运行时，不能误启桌面可执行文件。
    for candidate in &mut candidates {
        candidate.version = crate::cli_wrapper::probe_codex_cli_version(&candidate.path);
    }
    candidates.sort_by(compare_candidates);
    candidates
}

fn compare_candidates(left: &Candidate, right: &Candidate) -> std::cmp::Ordering {
    match (&left.version, &right.version) {
        (Some(left), Some(right)) => crate::cli_wrapper::compare_codex_versions(right, left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.ordinal.cmp(&right.ordinal),
    }
    .then_with(|| left.shell.cmp(&right.shell))
    .then_with(|| left.path.cmp(&right.path))
}

pub(crate) fn normalize_model(model: &Value) -> Option<Value> {
    if model.get("hidden").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let model_name = model
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| model.get("id").and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())?;
    let mut supported = model
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // 用户要求：没有桌面注入时，GPT 默认提供 Ultra；此补齐仅作用于 CLI 兜底，
    // 桌面界面增强仍按 Codex 的可见等级和功能开关独立处理。
    if model_name
        .trim()
        .rsplit('/')
        .next()
        .is_some_and(|slug| slug.to_ascii_lowercase().starts_with("gpt-"))
        && !supported
            .iter()
            .any(|effort| effort["reasoningEffort"] == "ultra")
    {
        supported.push(json!({
            "reasoningEffort": "ultra",
            "description": "Maximum reasoning with automatic task delegation"
        }));
    }
    let service_tiers = model
        .get("serviceTiers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let request_model = model
        .get("requestModel")
        .or_else(|| model.get("request_model"))
        .and_then(Value::as_str)
        .unwrap_or(model_name);
    // Fast 仅开放给实际请求的 GPT 模型及目录明确提供的档位；显式不支持、未知和非 GPT 保持关闭。
    let fast_supported = request_model
        .trim()
        .rsplit('/')
        .next()
        .is_some_and(|slug| slug.to_ascii_lowercase().starts_with("gpt-"))
        && !["supportsFast", "supports_fast", "fastModeSupported"]
            .iter()
            .any(|key| model.get(*key).and_then(Value::as_bool) == Some(false))
        && service_tiers
            .iter()
            .any(|tier| matches!(tier["id"].as_str(), Some("priority" | "fast")));
    Some(json!({
        "id": model.get("id").and_then(Value::as_str).unwrap_or(model_name),
        "model": model_name,
        "requestModel":request_model,
        "displayName": model.get("displayName").and_then(Value::as_str).unwrap_or(model_name),
        "supportedReasoningEfforts": supported,
        "defaultReasoningEffort": model.get("defaultReasoningEffort").cloned().unwrap_or(Value::Null),
        "serviceTiers":service_tiers,
        "fastModeSupported":fast_supported
    }))
}

fn service_history_items_page(page: &Value) -> (Vec<Value>, Option<String>) {
    let mut items = page
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|entry| entry.get("item").cloned().unwrap_or_else(|| entry.clone()))
        .collect::<Vec<_>>();
    items.reverse();
    let next_cursor = page
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    (items, next_cursor)
}

pub(crate) fn normalize_history_items_page(
    page: &Value,
    previous_cursor: Option<&str>,
) -> anyhow::Result<(Vec<Value>, Option<String>)> {
    let (items, next_cursor) = service_history_items_page(page);
    // 保留主线的分页防循环检查；历史项顺序和消息原始字段仍由新版解析逻辑处理。
    anyhow::ensure!(
        previous_cursor.is_none() || next_cursor.as_deref() != previous_cursor,
        "历史分页游标未前进"
    );
    Ok((items, next_cursor))
}

pub(crate) fn normalize_thread_response(response: &Value) -> anyhow::Result<Value> {
    let thread = response
        .get("thread")
        .context("Codex CLI service 未返回会话元数据")?;
    let mut normalized = normalize_thread(thread)?;
    if let Some(read_state) = response.get("readState") {
        normalized["readState"] = read_state.clone();
        if let Some(first_unread) = read_state.get("firstUnread") {
            normalized["hasUnreadTurn"] = json!(!first_unread.is_null());
        }
    }
    Ok(normalized)
}

pub(crate) fn normalize_thread(thread: &Value) -> anyhow::Result<Value> {
    let ephemeral = thread
        .get("ephemeral")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let parent_thread_id = thread
        .get("parentThreadId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let thread_source = thread
        .get("threadSource")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let source_is_subagent = thread
        .get("source")
        .and_then(Value::as_object)
        .is_some_and(|source| source.contains_key("subAgent") || source.contains_key("subagent"));
    if ephemeral
        || thread.get("attachedToThreadId").is_some_and(|parent| !parent.is_null())
        || parent_thread_id.is_some()
        || source_is_subagent
        || thread_source.is_some_and(|source| source.eq_ignore_ascii_case("subagent"))
        || thread.get("sideConversation") == Some(&Value::Bool(true))
    {
        bail!("Codex CLI service 只提供主会话元数据");
    }
    normalize_thread_metadata_with_preview(thread, true)
}

pub(crate) fn attached_parent_id(thread: &Value) -> Option<&str> {
    thread["attachedToThreadId"].as_str()
        .filter(|parent| !parent.is_empty() && thread["id"] != *parent)
}

pub(crate) fn normalize_session_response(response: &Value) -> anyhow::Result<Value> {
    if attached_parent_id(&response["thread"]).is_none() {
        return normalize_thread_response(response);
    }
    let raw = &response["thread"];
    anyhow::ensure!(raw["ephemeral"] == false && subagent_parent_id(raw).is_none(),
        "原生持久侧边身份无效");
    let mut value = normalize_thread_metadata(raw)?;
    value["viewKind"] = json!("side");
    value["serviceSide"] = json!(true);
    value["parentThreadId"] = raw["attachedToThreadId"].clone();
    value["attachedToThreadId"] = raw["attachedToThreadId"].clone();
    if let Some(read_state) = response.get("readState") {
        value["readState"] = read_state.clone();
        if let Some(first) = read_state.get("firstUnread") {
            value["hasUnreadTurn"] = json!(!first.is_null());
        }
    }
    Ok(value)
}

pub(crate) fn subagent_parent_id(thread: &Value) -> Option<&str> {
    thread["parentThreadId"]
        .as_str()
        .or_else(|| thread["source"]["subAgent"]["thread_spawn"]["parent_thread_id"].as_str())
        .or_else(|| thread["source"]["subagent"]["thread_spawn"]["parent_thread_id"].as_str())
        .filter(|parent| !parent.is_empty() && thread["id"] != *parent)
}

pub(crate) fn normalize_subagent_thread(thread: &Value) -> anyhow::Result<Value> {
    let parent = subagent_parent_id(thread).context("子智能体缺少有效父会话关系")?;
    anyhow::ensure!(
        thread["sideConversation"] != true,
        "侧边会话不能作为子智能体读取"
    );
    let mut value = normalize_thread_metadata(thread)?;
    // 用户要求服务模式恢复子智能体只读查看；此专用转换不改变普通会话和所有写入口的校验。
    value["readOnly"] = json!(true);
    value["viewKind"] = json!("subagent");
    value["parentThreadId"] = json!(parent);
    value["agentName"] = thread["agentNickname"]
        .as_str()
        .or_else(|| thread["source"]["subAgent"]["thread_spawn"]["agent_nickname"].as_str())
        .or_else(|| thread["source"]["subagent"]["thread_spawn"]["agent_nickname"].as_str())
        .or_else(|| thread["agentRole"].as_str())
        .map(|name| json!(name))
        .unwrap_or_else(|| value["title"].clone());
    value["priority"]["eligible"] = json!(false);
    value["requests"] = json!([]);
    value["requestsKnown"] = json!(true);
    value["queuedMessages"] = json!([]);
    value["queuedEdits"] = json!([]);
    value["editableMessage"] = Value::Null;
    Ok(value)
}

// 小于此值视为 Unix 秒；与远程会话服务的时间戳归一化规则
// 及 app.js 的 sidebarTimestamp() 使用同一阈值，三处需同步核对。
const UNIX_SECONDS_LIMIT: u64 = 100_000_000_000;

// 用户要求各实例会话的排序和日期分组使用相同单位：app-server 的
// createdAt / updatedAt 是 Unix 秒，网页和 Claude 会话为毫秒，统一输出毫秒。
// 已是毫秒的值（如侧边会话的 now_ms()）保持不变，重复规范化不会再次放大。
fn thread_timestamp_ms(value: Option<&Value>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    if let Some(at) = value.as_u64() {
        return json!(if at < UNIX_SECONDS_LIMIT {
            at.saturating_mul(1000)
        } else {
            at
        });
    }
    match value.as_f64() {
        Some(at) if at.is_finite() && at > 0.0 => json!(if at < UNIX_SECONDS_LIMIT as f64 {
            (at * 1000.0).round() as u64
        } else {
            at.round() as u64
        }),
        _ => value.clone(),
    }
}

pub(crate) fn normalize_thread_metadata(thread: &Value) -> anyhow::Result<Value> {
    normalize_thread_metadata_with_preview(thread, false)
}

fn normalize_thread_metadata_with_preview(
    thread: &Value,
    short_preview: bool,
) -> anyhow::Result<Value> {
    let id = thread["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .context("Codex CLI service 返回了无效会话 ID")?;
    let thread_source = thread["threadSource"].as_str();
    let cwd = thread
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("Codex CLI service 返回了无效会话工作目录")?;
    let title = thread
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            thread
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .map(std::borrow::Cow::Borrowed)
        .or_else(|| {
            thread
                .get("preview")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|preview| {
                    if short_preview {
                        std::borrow::Cow::Owned(provisional_title(preview))
                    } else {
                        std::borrow::Cow::Borrowed(preview)
                    }
                })
                .filter(|value| !value.is_empty())
        })
        .unwrap_or(std::borrow::Cow::Borrowed("新会话"));
    let status = thread.get("status").cloned().unwrap_or(Value::Null);
    let status_type = status.as_str().or_else(|| status["type"].as_str());
    let active = match status_type {
        Some("active" | "inProgress" | "running") => Some(true),
        Some("idle") => Some(false),
        _ => None,
    };
    let unread = thread.get("hasUnreadTurn").and_then(Value::as_bool);
    Ok(json!({
        "id": id,
        "title": title,
        "cwd": cwd,
        "createdAt": thread_timestamp_ms(thread.get("createdAt")),
        "updatedAt": thread_timestamp_ms(thread.get("updatedAt")),
        "recencyAt": thread_timestamp_ms(thread.get("recencyAt").or_else(|| thread.get("updatedAt"))),
        "model": thread.get("model").cloned().unwrap_or(Value::Null),
        "reasoningEffort": thread.get("reasoningEffort").cloned().unwrap_or(Value::Null),
        "status": status.clone(),
        "priority": {
            "status": status,
            "attentionState": match active { Some(true) => "active", Some(false) => "idle", None => "unknown" },
            "active": active,
            "eligible": true
        },
        "hasUnreadTurn": unread,
        "lastTurnOutcome": thread.get("lastTurnOutcome").cloned().unwrap_or(Value::Null),
        "readOnly": false,
        "viewKind": "main",
        "parentThreadId": Value::Null,
        "ephemeral": thread.get("ephemeral").cloned().unwrap_or(Value::Null),
        "threadSource": thread_source,
        "source": thread.get("source").cloned().unwrap_or(Value::Null),
        "requests": thread.get("requests").cloned().unwrap_or(Value::Null),
        "requestsKnown": thread.get("requests").is_some_and(Value::is_array),
        "queuedMessages": Value::Null,
        "queuedEdits": Value::Null,
        "transcriptEvents": []
    }))
}

fn provisional_title(preview: &str) -> String {
    use pulldown_cmark::{Event, Parser, TagEnd};

    // 用户要求对齐 Codex App 的临时标题：正文转单行纯文本，60 字符含省略号。
    // 只处理未命名的 Codex 主会话预览；正式标题、侧边会话和原消息内容不变。
    const MAX_PREVIEW_CHARS: usize = 60;
    let mut plain = String::new();
    for event in Parser::new(preview) {
        match event {
            Event::Text(text) | Event::Code(text) => plain.push_str(&text),
            Event::SoftBreak
            | Event::HardBreak
            | Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item | TagEnd::CodeBlock,
            ) => plain.push(' '),
            _ => {}
        }
    }
    let one_line = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = one_line.char_indices();
    if let Some((end, _)) = chars.nth(MAX_PREVIEW_CHARS - 1)
        && chars.next().is_some()
    {
        return format!("{}…", one_line[..end].trim_end());
    }
    one_line
}

fn truncate_error(value: &str) -> String {
    const MAX_ERROR_CHARS: usize = 1_000;
    value.chars().take(MAX_ERROR_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_thread_metadata_preserves_preview_object_status_and_outer_read_state() {
        let raw = json!({
            "thread":{"id":"main","name":null,"preview":"真实首条消息",
                "cwd":"C:/workspace","source":"vscode","threadSource":"user",
                "ephemeral":false,"parentThreadId":null,"status":{"type":"active"}},
            "readState":{"firstUnread":{"turnId":"unread-turn"},"revision":21}
        });
        let normalized = normalize_thread_response(&raw).unwrap();
        assert_eq!(normalized["title"], "真实首条消息");
        assert_eq!(normalized["priority"]["active"], true);
        assert_eq!(normalized["hasUnreadTurn"], true);
        assert_eq!(normalized["readState"], raw["readState"]);
        let mut long = raw.clone();
        let text = format!(
            "# **统计** [数字人功能](https://example.com/tasks/1)\n\n{}",
            "字".repeat(70)
        );
        long["thread"]["preview"] = json!(text);
        let short = normalize_thread_response(&long).unwrap();
        let short = short["title"].as_str().unwrap();
        assert!(short.starts_with("统计 数字人功能 "));
        assert!(short.ends_with('…'));
        assert_eq!(short.chars().count(), 60);
        assert!(!short.contains("https"));
        assert_eq!(
            normalize_thread_metadata(&long["thread"]).unwrap()["title"],
            long["thread"]["preview"]
        );
        long["thread"]["name"] = json!("已保存".repeat(30));
        assert_eq!(
            normalize_thread_response(&long).unwrap()["title"],
            long["thread"]["name"]
        );
        let mut unloaded = raw;
        unloaded["thread"]["status"]["type"] = json!("notLoaded");
        unloaded.as_object_mut().unwrap().remove("readState");
        let unknown = normalize_thread_response(&unloaded).unwrap();
        assert!(unknown["priority"]["active"].is_null());
        assert_eq!(unknown["priority"]["attentionState"], "unknown");
        assert!(unknown["hasUnreadTurn"].is_null());
    }

    #[test]
    fn service_history_items_page_restores_order_and_preserves_payloads_and_cursor() {
        let user = json!({
            "type":"userMessage","id":"user-1","clientId":"client-1",
            "content":[{"type":"text","text":"测试消息","text_elements":[]}]
        });
        let reasoning = json!({
            "type":"reasoning","id":"reasoning-1","summary":[],"content":[]
        });
        let assistant = json!({
            "type":"agentMessage","id":"assistant-1","text":"完整回复",
            "phase":"final_answer","memoryCitation":null,"delivery":null,"questions":null
        });
        let expected = vec![user.clone(), reasoning.clone(), assistant.clone()];
        let cursor = r#"{"requestedThreadId":"main","rolloutOrdinal":30,"scope":{"kind":"items","turnId":"turn-1"}}"#;
        for page in [
            json!({
                "data":[
                    {"item":assistant,"turnId":"turn-1"},
                    {"item":reasoning,"turnId":"turn-1"},
                    {"item":user,"turnId":"turn-1"}
                ],
                "nextCursor":cursor
            }),
            json!({"data":[assistant,reasoning,user],"nextCursor":cursor}),
        ] {
            let (items, next_cursor) = service_history_items_page(&page);
            assert_eq!(items, expected);
            assert_eq!(next_cursor.as_deref(), Some(cursor));
        }
    }

    #[test]
    fn normalize_model_keeps_ultra_and_drops_hidden_entries() {
        let model = normalize_model(&json!({
            "id": "gpt-6-astra",
            "model": "gpt-6-astra",
            "displayName": "GPT-6 Astra",
            "hidden": false,
            "supportedReasoningEfforts": [
                {"reasoningEffort": "medium"},
                {"reasoningEffort": "ultra"}
            ],
            "defaultReasoningEffort": "medium"
        }))
        .unwrap();
        assert_eq!(model["model"], "gpt-6-astra");
        assert_eq!(
            model["supportedReasoningEfforts"][1]["reasoningEffort"],
            "ultra"
        );
        assert!(normalize_model(&json!({"model":"hidden","hidden":true})).is_none());
    }

    #[test]
    fn service_model_catalog_adds_ultra_only_for_gpt_without_changing_defaults() {
        for model in ["gpt-5.5", "gpt-6.1-sol", "openai/gpt-6.1-terra"] {
            let original = json!({
                "model": model,
                "defaultReasoningEffort": "xhigh",
                "supportedReasoningEfforts": [{"reasoningEffort": "xhigh"}]
            });
            let normalized = normalize_model(&original).unwrap();
            assert_eq!(normalized["defaultReasoningEffort"], "xhigh");
            assert_eq!(
                normalized["supportedReasoningEfforts"][0]["reasoningEffort"],
                "xhigh"
            );
            assert_eq!(
                normalized["supportedReasoningEfforts"][1]["reasoningEffort"],
                "ultra"
            );
            assert_eq!(normalize_model(&normalized).unwrap(), normalized);
            assert_eq!(
                original["supportedReasoningEfforts"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
        for model in ["claude-opus-5", "kimi-k3", "o3", "gptish-model"] {
            let normalized = normalize_model(&json!({
                "model": model,
                "supportedReasoningEfforts": [{"reasoningEffort": "high"}]
            }))
            .unwrap();
            assert_eq!(
                normalized["supportedReasoningEfforts"],
                json!([{"reasoningEffort": "high"}])
            );
        }
    }

    #[test]
    fn normalize_model_uses_id_when_model_name_is_missing() {
        let model = normalize_model(&json!({
            "id": "gpt-fallback",
            "supportedReasoningEfforts": []
        }))
        .unwrap();
        assert_eq!(model["model"], "gpt-fallback");
        assert_eq!(model["displayName"], "gpt-fallback");
    }

    #[test]
    fn normalize_thread_keeps_main_session_metadata_and_rejects_ephemeral_sessions() {
        let thread = normalize_thread(&json!({
            "id": "thread-1",
            "name": "主会话",
            "cwd": "C:/workspace",
            "createdAt": 1,
            "updatedAt": 2,
            "status": "idle",
            "ephemeral": false
        }))
        .unwrap();
        assert_eq!(thread["id"], "thread-1");
        assert_eq!(thread["title"], "主会话");
        assert_eq!(thread["viewKind"], "main");
        assert_eq!(thread["createdAt"], 1000);
        assert_eq!(thread["updatedAt"], 2000);
        assert_eq!(thread["recencyAt"], 2000);
        // app-server 返回 Unix 秒；侧栏日期分组与排序按毫秒比较，已是毫秒的值不能再放大。
        let seconds = normalize_thread(&json!({
            "id": "thread-seconds", "cwd": "C:/workspace",
            "createdAt": 1792319939, "updatedAt": 1792320819
        }))
        .unwrap();
        assert_eq!(seconds["createdAt"], 1_792_319_939_000_u64);
        assert_eq!(seconds["updatedAt"], 1_792_320_819_000_u64);
        assert_eq!(seconds["recencyAt"], 1_792_320_819_000_u64);
        let millis = normalize_thread_metadata(&seconds).unwrap();
        assert_eq!(millis["createdAt"], 1_792_319_939_000_u64);
        assert_eq!(millis["updatedAt"], 1_792_320_819_000_u64);
        assert!(
            normalize_thread(&json!({
                "id": "side-1",
                "cwd": "C:/workspace",
                "ephemeral": true
            }))
            .is_err()
        );
        let attached = json!({"id":"persistent-side","cwd":"C:/workspace","ephemeral":false,
            "attachedToThreadId":"main","parentThreadId":null});
        assert!(normalize_thread(&attached).is_err(), "持久侧边不得进入普通主会话目录");
        let normalized = normalize_session_response(&json!({"thread":attached,
            "readState":{"firstUnread":{"turnId":"side-turn"},"revision":"r1"}})).unwrap();
        assert_eq!(normalized["viewKind"], "side");
        assert_eq!(normalized["ephemeral"], false);
        assert_eq!(normalized["parentThreadId"], "main");
        assert_eq!(normalized["hasUnreadTurn"], true);
        for invalid in [json!(""), json!("persistent-side"), json!(42)] {
            let mut malformed = attached.clone();
            malformed["attachedToThreadId"] = invalid;
            assert!(normalize_session_response(&json!({"thread":malformed})).is_err());
        }
    }

    #[test]
    fn service_history_rejects_a_repeated_item_cursor() {
        let error = normalize_history_items_page(
            &json!({"data": [], "nextCursor": "same-cursor"}),
            Some("same-cursor"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("历史分页游标未前进"));
    }

    #[test]
    fn service_candidates_prefer_newest_cli_runtime() {
        let mut candidates = vec![
            Candidate {
                path: PathBuf::from("codex-old.exe"),
                shell: false,
                version: crate::cli_wrapper::parse_codex_cli_version("codex-cli 0.130.0-alpha.5"),
                ordinal: 0,
            },
            Candidate {
                path: PathBuf::from("codex-current.exe"),
                shell: false,
                version: crate::cli_wrapper::parse_codex_cli_version("codex-cli 0.166.0-alpha.5.1"),
                ordinal: 1,
            },
        ];
        candidates.sort_by(compare_candidates);
        assert_eq!(candidates[0].path, PathBuf::from("codex-current.exe"));
    }

    #[test]
    fn daemon_version_confirms_the_expected_socket() {
        let socket =
            PathBuf::from(r"C:\Users\me\.codex\app-server-control\app-server-control.sock");
        assert!(daemon_version_confirms_socket(
            &json!({
                "status": "running",
                "socketPath": "c:/Users/me/.codex/app-server-control/app-server-control.sock"
            }),
            &socket
        ));
        assert!(!daemon_version_confirms_socket(
            &json!({
                "status": "stopped",
                "socketPath": socket
            }),
            &socket
        ));
        assert!(!daemon_version_confirms_socket(
            &json!({
                "status": "running",
                "socketPath": r"C:\Users\other\.codex\app-server-control\app-server-control.sock"
            }),
            &socket
        ));
    }

    #[test]
    fn service_transport_only_allows_daemon_proxy_handoff() {
        assert!(ServiceTransport::DaemonProxy.supports_handoff());
        assert!(!ServiceTransport::Stdio.supports_handoff());
        assert_eq!(
            ServiceTransport::DaemonProxy.execution_label(),
            "app-server-daemon-proxy"
        );
        assert_eq!(
            ServiceTransport::Stdio.execution_label(),
            "app-server-stdio"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn missing_daemon_manifest_is_recovered_from_cli_state() {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let socket = daemon_socket_path(&codex_home);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, b"socket-placeholder").unwrap();
        let socket_path = socket.to_string_lossy().replace('\\', "/");
        let version_json = format!(
            r#"{{"status":"running","socketPath":"{socket_path}","managedCodexPath":"managed-codex","managedCodexVersion":"0.166.0","appServerVersion":"0.166.0"}}"#
        );
        let candidate_path = directory.path().join("codex-stub.cmd");
        std::fs::write(
            &candidate_path,
            format!(
                "@echo off\r\nif \"%1\"==\"app-server\" if \"%2\"==\"daemon\" if \"%3\"==\"version\" (echo {version_json}\r\n exit /b 0\r\n)\r\nexit /b 17\r\n"
            ),
        )
        .unwrap();
        let candidate = Candidate {
            path: candidate_path,
            shell: true,
            version: crate::cli_wrapper::parse_codex_cli_version("codex-cli 0.166.0"),
            ordinal: 0,
        };

        let resolved = ensure_daemon(&candidate, &codex_home).await.unwrap();
        assert_eq!(resolved, socket);
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(codex_home.join(DAEMON_MANIFEST_FILE)).unwrap())
                .unwrap();
        assert_eq!(manifest["cliPath"], candidate.path.to_string_lossy().as_ref());
        assert_eq!(manifest["cliVersion"], "0.166.0");
        assert_eq!(manifest["managedCodexVersion"], "0.166.0");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn existing_daemon_socket_failure_does_not_fallback_to_stdio() {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex-home");
        let socket = daemon_socket_path(&codex_home);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, b"socket-placeholder").unwrap();
        let candidate_path = directory.path().join("codex-stub.cmd");
        std::fs::write(&candidate_path, "@echo off\r\nexit /b 17\r\n").unwrap();
        let candidate = Candidate {
            path: candidate_path,
            shell: true,
            version: crate::cli_wrapper::parse_codex_cli_version("codex-cli 0.166.0"),
            ordinal: 0,
        };

        let error = match spawn_candidate(&candidate, &codex_home).await {
            Ok(_) => panic!("已有 daemon 套接字核验失败后不应降级到 stdio"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("运行中的套接字未通过 CLI 状态核对"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_batch_candidate_preserves_arguments_output_and_exit_status() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("Codex CLI & wrapper.cmd");
        std::fs::write(
            &path,
            "@echo off\r\necho arguments=%*\r\necho service-stderr 1>&2\r\nexit /b 19\r\n",
        )
        .unwrap();
        let output = candidate_command(&Candidate {
            path,
            shell: true,
            version: None,
            ordinal: 0,
        })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
        assert_eq!(output.status.code(), Some(19));
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("arguments=app-server --analytics-default-enabled")
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("service-stderr"));
    }
}
